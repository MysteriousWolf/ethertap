#[cfg(not(target_os = "macos"))]
use crossbeam_channel::tick;
use crossbeam_channel::{Receiver, Sender, bounded};
use std::sync::Arc;
/// MIDI device hot-plug watcher.
///
/// On macOS this uses native CoreMIDI notification callbacks via
/// `coremidi::Client::new_with_notifications` + a CFRunLoop on a dedicated
/// thread — zero polling.  On non-macOS (Linux / Windows) it falls back to
/// periodic polling via `midir::MidiOutput`.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

/// Length of one CFRunLoop slice on macOS — also serves as the debounce
/// window: a notification observed anywhere within a slice is rebroadcast at
/// the following slice boundary, so a trailing "device is back" notification
/// can never be permanently suppressed the way the old cooldown-with-`return`
/// design allowed.
#[cfg(target_os = "macos")]
const SLICE_MS: u64 = 300;

/// Safety re-poll interval — re-enumerates and broadcasts only if the device
/// list actually changed since the last broadcast. Heals notification
/// classes CoreMIDI never fires for (e.g. MIDIServer restart) and self-heals
/// any broadcast dropped by a full `try_send` channel (the channel keeps its
/// bounded(16) size; a dropped send is recovered within one safety-poll
/// interval instead of adding consumer-side plumbing).
///
/// Not cfg-gated: `BroadcastPlanner` below is platform-independent so it can
/// be unit-tested on every CI runner; only its macOS wiring is cfg-gated.
const SAFETY_POLL_MS: u64 = 15_000;

// ─── Broadcast decision logic (platform-independent, unit-testable) ────────────

/// What a slice-boundary tick should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickAction {
    /// Broadcast `current` to both channels and record it as the last-sent
    /// list.
    Rebroadcast,
    /// The safety poll ran but the list matches the last broadcast — no
    /// channel send needed.
    SkipUnchanged,
}

/// Owns the debounce + safety-poll decision for the macOS CoreMIDI watcher.
/// Per-watcher-thread state — each `spawn_macos` invocation gets its own
/// instance, unlike the old `static LAST_MS: OnceLock<AtomicU64>` which was
/// shared process-wide and let multiple plugin instances suppress each
/// other's broadcasts.
struct BroadcastPlanner {
    /// Set by the CoreMIDI notification callback; cleared on the next
    /// `decide()` call. A notification observed anywhere within a slice
    /// guarantees a rebroadcast at the following boundary — the trailing
    /// edge can never be silently dropped the way the old cooldown-with-
    /// `return` design allowed.
    dirty: bool,
    /// Device list from the last broadcast (initial seed excluded — see
    /// `spawn_macos`, which sends the seed itself before the slice loop
    /// starts and primes this via `record_broadcast`).
    last_sent: Vec<String>,
    /// `now_ms` at or after which the safety re-poll should run again.
    next_safety_due_ms: u64,
}

impl BroadcastPlanner {
    fn new(now_ms: u64) -> Self {
        Self {
            dirty: false,
            last_sent: Vec::new(),
            next_safety_due_ms: now_ms.saturating_add(SAFETY_POLL_MS),
        }
    }

    /// Called from the CoreMIDI notification callback (or a test) to mark a
    /// topology change observed since the last tick.
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether this slice boundary needs the caller to enumerate the device
    /// list at all. Idle boundaries (no notification, safety poll not due)
    /// return `false` and the caller can skip straight to the next slice
    /// without touching CoreMIDI.
    fn needs_enumeration(&self, now_ms: u64) -> bool {
        self.dirty || now_ms >= self.next_safety_due_ms
    }

    /// Consumes the freshly enumerated `current` device list and decides the
    /// action. Must only be called when `needs_enumeration(now_ms)` was
    /// `true` for the same `now_ms`.
    fn decide(&mut self, now_ms: u64, current: &[String]) -> TickAction {
        let was_dirty = self.dirty;
        self.dirty = false;
        if now_ms >= self.next_safety_due_ms {
            self.next_safety_due_ms = now_ms.saturating_add(SAFETY_POLL_MS);
        }
        // A dirty tick always rebroadcasts, even if `current` happens to
        // match `last_sent` — a mid-transition enumeration that coincides
        // with the last broadcast must not suppress the real update that
        // triggered the notification.
        if was_dirty || current != self.last_sent.as_slice() {
            self.last_sent = current.to_vec();
            TickAction::Rebroadcast
        } else {
            TickAction::SkipUnchanged
        }
    }

    /// Records a broadcast made outside `decide()` — used to prime
    /// `last_sent` with the initial seed broadcast sent before the slice
    /// loop starts.
    #[cfg(target_os = "macos")]
    fn record_broadcast(&mut self, list: Vec<String>) {
        self.last_sent = list;
    }
}

/// Polling interval for the non-macOS fallback. Not cfg-gated so the editor
/// (compiled for all platforms) can reference it for the status row even
/// when the runtime platform check picks the macOS (event-driven) branch.
pub const POLL_INTERVAL_SECS: u64 = 2;

// ─── Public types ──────────────────────────────────────────────────────────────

/// Channels for MIDI device list updates.
///
/// Both receivers get the same `Vec<String>` whenever the available MIDI
/// output ports change (device plugged / unplugged).
pub struct MidiWatcherChannels {
    /// For the GUI editor — replaces 2 s editor polling.
    pub editor_rx: Receiver<Vec<String>>,
    /// For the MIDI clock worker — triggers immediate reconnection attempts.
    pub worker_rx: Receiver<Vec<String>>,
    /// Set to `true` to request the polling thread to exit on next tick.
    /// macOS uses CFRunLoop and cannot be interrupted; the OS cleans it up on
    /// process exit.
    pub shutdown: Arc<AtomicBool>,
    /// Millisecond timestamp of the last device-list broadcast (0 = never).
    /// Updated on every broadcast in both platform paths, including the
    /// initial seed — lets the editor show "updated Xs ago" status.
    pub last_update_ts: Arc<AtomicU64>,
    /// `true` once the initial device-list broadcast has landed on either
    /// platform path. `last_update_ts` alone can't serve as the "never
    /// updated" sentinel because `crate::network::now_ms()` returns 0 on its
    /// first process-wide call — if that happens to be the seed store,
    /// `last_update_ts` would be 0 even though the broadcast already landed.
    /// Write-once-true, never reset to `false`.
    pub has_update: Arc<AtomicBool>,
}

/// Spawns a background device watcher and returns two receivers (editor +
/// worker) that each receive updated device-name lists when the MIDI port
/// topology changes.
pub fn spawn() -> MidiWatcherChannels {
    let (ed_tx, ed_rx) = bounded::<Vec<String>>(16);
    let (wk_tx, wk_rx) = bounded::<Vec<String>>(16);
    let shutdown = Arc::new(AtomicBool::new(false));
    let last_update_ts = Arc::new(AtomicU64::new(0));
    let has_update = Arc::new(AtomicBool::new(false));

    #[cfg(target_os = "macos")]
    spawn_macos(ed_tx, wk_tx, last_update_ts.clone(), has_update.clone());

    #[cfg(not(target_os = "macos"))]
    spawn_polling(
        ed_tx,
        wk_tx,
        shutdown.clone(),
        last_update_ts.clone(),
        has_update.clone(),
    );

    MidiWatcherChannels {
        editor_rx: ed_rx,
        worker_rx: wk_rx,
        shutdown,
        last_update_ts,
        has_update,
    }
}

// ─── macOS: CoreMIDI notification watcher ──────────────────────────────────────

/// Enumerate CoreMIDI output destinations and return display names, filtering
/// out the EtherTap virtual clock port.
#[cfg(target_os = "macos")]
fn enumerate_devices() -> Vec<String> {
    use coremidi::Destinations;
    Destinations
        .into_iter()
        .filter_map(|d| d.display_name())
        .filter(|n| n != "EtherTap MIDI Clock")
        .collect()
}

#[cfg(target_os = "macos")]
fn spawn_macos(
    ed_tx: Sender<Vec<String>>,
    wk_tx: Sender<Vec<String>>,
    last_update_ts: Arc<AtomicU64>,
    has_update: Arc<AtomicBool>,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("ethertap-midi-watch".into())
        .spawn(move || {
            use core_foundation::runloop::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
            use coremidi::{Client, Notification};
            use std::time::Duration;

            // Notification callback is intentionally minimal: no
            // enumeration, no channel sends, no cooldown state. It only
            // flags that *something* changed; the slice loop below decides
            // when to re-enumerate and broadcast. This removes the old
            // cooldown-with-`return` bug where an early mid-transition
            // enumeration would broadcast and then permanently suppress the
            // trailing "device is back" notification.
            let dirty = Arc::new(AtomicBool::new(false));
            let dirty_cb = dirty.clone();
            let _client = match Client::new_with_notifications(
                "EtherTap-MIDI-Watch",
                move |notification: &Notification| {
                    if matches!(
                        notification,
                        Notification::ObjectAdded(_) | Notification::ObjectRemoved(_)
                    ) {
                        dirty_cb.store(true, Ordering::Relaxed);
                    }
                },
            ) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("[EtherTap] CoreMIDI notification client: {e}");
                    return;
                }
            };

            // Initial enumeration — CoreMIDI does NOT fire ObjectAdded for
            // already-connected devices, so we must seed the channel once
            // before entering the slice loop. Without this the editor never
            // discovers existing MIDI ports.
            let initial_devices = enumerate_devices();
            // Stamp before sending — a receiver that observes the broadcast
            // must also observe the timestamp; the channel send/recv pair is
            // what publishes these Relaxed stores.
            last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
            has_update.store(true, Ordering::Relaxed);
            let _ = ed_tx.try_send(initial_devices.clone());
            let _ = wk_tx.try_send(initial_devices.clone());

            let mut planner = BroadcastPlanner::new(crate::network::now_ms());
            planner.record_broadcast(initial_devices);

            loop {
                // A slice IS the debounce window: any notification landing
                // during it is observed as `dirty` at the following
                // boundary, so a trailing notification can never be
                // silently dropped the way the old cooldown-with-`return`
                // design allowed.
                let run_result = CFRunLoop::run_in_mode(
                    unsafe { kCFRunLoopDefaultMode },
                    Duration::from_millis(SLICE_MS),
                    false,
                );
                // With zero registered sources the run loop returns
                // immediately instead of blocking for the slice, which would
                // turn this loop into a busy-spin. The CoreMIDI notification
                // source registered above makes that unreachable today; the
                // sleep keeps the thread bounded if that invariant ever
                // breaks (coremidi upgrade, refactor).
                if run_result == CFRunLoopRunResult::Finished {
                    std::thread::sleep(Duration::from_millis(SLICE_MS));
                }

                if dirty.swap(false, Ordering::Relaxed) {
                    planner.mark_dirty();
                }

                let now = crate::network::now_ms();
                if !planner.needs_enumeration(now) {
                    continue;
                }

                let devices = enumerate_devices();
                if planner.decide(now, &devices) == TickAction::Rebroadcast {
                    // Stamp before sending — see the initial-seed broadcast
                    // above for why.
                    last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
                    has_update.store(true, Ordering::Relaxed);
                    // `try_send` on a full channel silently drops this
                    // broadcast (the editor drains only while its GUI is
                    // open). No consumer-side plumbing needed to fix that:
                    // the unconditional rebroadcast-on-change above means
                    // the drop self-heals within one `SAFETY_POLL_MS`
                    // window at the latest.
                    if ed_tx.try_send(devices.clone()).is_err() {
                        log::debug!(
                            "[EtherTap] MIDI device list: editor channel full, broadcast dropped (self-heals via safety poll)"
                        );
                    }
                    if wk_tx.try_send(devices).is_err() {
                        log::debug!(
                            "[EtherTap] MIDI device list: worker channel full, broadcast dropped (self-heals via safety poll)"
                        );
                    }
                }
            }
        })
    {
        log::error!("[EtherTap] failed to spawn MIDI watcher thread: {e}");
    }
}

// ─── Non-macOS: polling fallback ─────────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
fn scan_ports() -> Vec<String> {
    match midir::MidiOutput::new("EtherTap-Scan") {
        Ok(out) => out
            .ports()
            .iter()
            .filter_map(|p| out.port_name(p).ok())
            .filter(|n| n != "EtherTap MIDI Clock")
            .collect(),
        Err(e) => {
            log::warn!(
                "[EtherTap] MIDI port scan failed: {e} \
                        (is ALSA/JACK available?)"
            );
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_polling(
    ed_tx: Sender<Vec<String>>,
    wk_tx: Sender<Vec<String>>,
    shutdown: Arc<AtomicBool>,
    last_update_ts: Arc<AtomicU64>,
    has_update: Arc<AtomicBool>,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("ethertap-midi-watch".into())
        .spawn(move || {
            // Send initial device list immediately, mirroring the macOS path.
            // Without this the MIDI clock worker starts with an empty port list
            // and the first connection attempt cannot succeed until the first
            // timer tick (POLL_INTERVAL_SECS seconds later).
            let initial = scan_ports();
            // Stamp before sending: a receiver that observes the broadcast
            // must also observe the timestamp. The channel send/recv pair is
            // what publishes these Relaxed stores.
            last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
            has_update.store(true, Ordering::Relaxed);
            let _ = ed_tx.try_send(initial.clone());
            let _ = wk_tx.try_send(initial);

            let scan_timer = tick(Duration::from_secs(POLL_INTERVAL_SECS));
            loop {
                let _ = scan_timer.recv();
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let devices = scan_ports();
                last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
                has_update.store(true, Ordering::Relaxed);
                let _ = ed_tx.try_send(devices.clone());
                let _ = wk_tx.try_send(devices);
            }
        })
    {
        log::error!("[EtherTap] failed to spawn MIDI poll thread: {e}");
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A notification observed anywhere within a slice must produce
    /// `Rebroadcast` at the next tick — this is the trailing-edge-never-
    /// dropped guarantee that replaces the old cooldown-with-`return` bug.
    /// Holds even when the enumerated list happens to match the last
    /// broadcast: a mid-transition enumeration coinciding with `last_sent`
    /// must not suppress the real update that triggered the notification.
    #[test]
    fn dirty_notification_forces_rebroadcast_on_next_tick() {
        let mut planner = BroadcastPlanner::new(0);
        planner.decide(0, &names(&["A"])); // prime last_sent = ["A"]

        planner.mark_dirty();
        assert!(planner.needs_enumeration(100));
        assert_eq!(
            planner.decide(100, &names(&["A"])),
            TickAction::Rebroadcast,
            "dirty tick must rebroadcast even with an unchanged list"
        );
    }

    /// Safety poll: not due before `SAFETY_POLL_MS` elapses (no notification
    /// pending), then due afterward. A changed list rebroadcasts; an
    /// unchanged list is a no-op (no channel churn from the periodic check).
    #[test]
    fn safety_poll_rebroadcasts_on_change_and_skips_when_unchanged() {
        let mut planner = BroadcastPlanner::new(0);
        planner.decide(0, &names(&["A"])); // prime last_sent = ["A"]

        assert!(
            !planner.needs_enumeration(SAFETY_POLL_MS - 1),
            "safety poll must not fire before its interval elapses"
        );

        assert!(planner.needs_enumeration(SAFETY_POLL_MS));
        assert_eq!(
            planner.decide(SAFETY_POLL_MS, &names(&["A", "B"])),
            TickAction::Rebroadcast,
            "safety poll must rebroadcast when the list changed"
        );

        let next_due = SAFETY_POLL_MS * 2;
        assert!(planner.needs_enumeration(next_due));
        assert_eq!(
            planner.decide(next_due, &names(&["A", "B"])),
            TickAction::SkipUnchanged,
            "safety poll must not rebroadcast an unchanged list"
        );
    }

    /// Regression for the old `static LAST_MS: OnceLock<AtomicU64>` — two
    /// planner instances (as created by two plugin instances) must not share
    /// dirty/timing state.
    #[test]
    fn two_planners_do_not_share_state() {
        let mut planner_a = BroadcastPlanner::new(0);
        let planner_b = BroadcastPlanner::new(0);

        planner_a.mark_dirty();

        assert!(planner_a.needs_enumeration(50));
        assert!(
            !planner_b.needs_enumeration(50),
            "planner_b must not observe planner_a's dirty flag"
        );
    }

    /// `spawn()` seeds `last_update_ts` and `has_update` from the initial
    /// device-list broadcast on both platform paths (macOS CoreMIDI callback
    /// path, non-macOS polling path) — without this the MIDI picker modal
    /// would show "waiting for devices…" forever even after the first
    /// enumeration completes.
    #[test]
    fn spawn_seeds_last_update_ts() {
        let channels = spawn();

        // The watcher thread stamps last_update_ts + has_update and *then*
        // sends its initial broadcast, so receiving the broadcast is what
        // makes both stores observable here — the send/recv pair supplies the
        // ordering the Relaxed stores lack. (A value of 0 for last_update_ts
        // is itself a valid, legitimate `now_ms()` reading taken very early in
        // process lifetime, so it can't be used as an "unset" sentinel.)
        //
        // The timeout is generous because the macOS path builds a CoreMIDI
        // notification client first, which can take seconds on a loaded
        // machine; this test gates on ordering, not on latency.
        channels
            .editor_rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("initial device-list broadcast was never sent within 15s of spawn()");

        assert!(
            channels.has_update.load(Ordering::Relaxed),
            "has_update must be true after the initial device-list broadcast"
        );

        let ts = channels.last_update_ts.load(Ordering::Relaxed);

        // `last_update_ts` must be on the same monotonic base as
        // `crate::network::now_ms()` (used by `scan_completed_ts` and the
        // editor's age computation) — NOT a `SystemTime` epoch ms value,
        // which would be orders of magnitude larger and make
        // `now_ms().saturating_sub(ts)` clamp to 0 forever.
        let now = crate::network::now_ms();
        assert!(
            now >= ts,
            "last_update_ts ({ts}) is ahead of now_ms() ({now}) — wrong time base"
        );
        assert!(
            now - ts < 15_000,
            "last_update_ts ({ts}) is not within 15s of now_ms() ({now}) — wrong time base"
        );

        // Request shutdown so the non-macOS polling thread exits promptly;
        // macOS uses CFRunLoop and is cleaned up by the OS on process exit.
        channels.shutdown.store(true, Ordering::Release);
    }
}
