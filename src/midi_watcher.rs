#[cfg(not(target_os = "macos"))]
use crossbeam_channel::tick;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
/// MIDI device hot-plug watcher.
///
/// On macOS this uses native CoreMIDI notification callbacks via
/// `coremidi::Client::new_with_notifications` + a CFRunLoop on a dedicated
/// thread — zero polling.  On non-macOS (Linux / Windows) it falls back to
/// periodic polling via `midir::MidiOutput`.
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
#[cfg(not(target_os = "macos"))]
use std::time::Duration;

/// Minimum interval between device-list broadcasts — rate-limits flurries of
/// CoreMIDI notifications during USB hub plug/unplug.
#[cfg(target_os = "macos")]
const BROADCAST_COOLDOWN_MS: u64 = 300;

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

/// Returns the current time as milliseconds since the Unix epoch (0 on clock
/// error). Used only for the CoreMIDI cooldown check, which is
/// self-consistent on its own `SystemTime` base.
#[cfg(target_os = "macos")]
fn now_ms_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
            use core_foundation::runloop::CFRunLoop;
            use coremidi::{Client, Notification};

            // Clone Senders before the move closure so we can also send
            // the initial device list before entering the run loop.
            let (ed_tx_cb, wk_tx_cb) = (ed_tx.clone(), wk_tx.clone());
            let last_update_ts_cb = last_update_ts.clone();
            let has_update_cb = has_update.clone();
            let _client = match Client::new_with_notifications(
                "EtherTap-MIDI-Watch",
                move |notification: &Notification| {
                    if !matches!(
                        notification,
                        Notification::ObjectAdded(_) | Notification::ObjectRemoved(_)
                    ) {
                        return;
                    }

                    // Cooldown — CoreMIDI may fire multiple notifications for
                    // a single physical plug/unplug (USB hub topology). Kept
                    // on its own self-consistent SystemTime base.
                    static LAST_MS: OnceLock<AtomicU64> = OnceLock::new();
                    let now = now_ms_epoch();
                    {
                        let last = LAST_MS.get_or_init(|| AtomicU64::new(0));
                        let prev = last.load(Ordering::Relaxed);
                        if now.saturating_sub(prev) < BROADCAST_COOLDOWN_MS {
                            return;
                        }
                        last.store(now, Ordering::Relaxed);
                    }

                    let devices = enumerate_devices();
                    let _ = ed_tx_cb.try_send(devices.clone());
                    let _ = wk_tx_cb.try_send(devices);
                    last_update_ts_cb.store(crate::network::now_ms(), Ordering::Relaxed);
                    has_update_cb.store(true, Ordering::Relaxed);
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
            // before entering the run loop.  Without this the editor never
            // discovers existing MIDI ports.
            let initial_devices = enumerate_devices();
            let _ = ed_tx.try_send(initial_devices.clone());
            let _ = wk_tx.try_send(initial_devices);
            last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
            has_update.store(true, Ordering::Relaxed);

            // _client kept alive for its Drop — unregisters the CoreMIDI
            // notification callback when the thread exits.
            CFRunLoop::run_current();
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
            let _ = ed_tx.try_send(initial.clone());
            let _ = wk_tx.try_send(initial);
            last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
            has_update.store(true, Ordering::Relaxed);

            let scan_timer = tick(Duration::from_secs(POLL_INTERVAL_SECS));
            loop {
                let _ = scan_timer.recv();
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let devices = scan_ports();
                let _ = ed_tx.try_send(devices.clone());
                let _ = wk_tx.try_send(devices);
                last_update_ts.store(crate::network::now_ms(), Ordering::Relaxed);
                has_update.store(true, Ordering::Relaxed);
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

    /// `spawn()` seeds `last_update_ts` and `has_update` from the initial
    /// device-list broadcast on both platform paths (macOS CoreMIDI callback
    /// path, non-macOS polling path) — without this the MIDI picker modal
    /// would show "waiting for devices…" forever even after the first
    /// enumeration completes.
    #[test]
    fn spawn_seeds_last_update_ts() {
        let channels = spawn();

        // The watcher thread sends its initial broadcast and then stamps
        // last_update_ts + has_update, but it's async — wait for the
        // broadcast to land on editor_rx (a value of 0 for last_update_ts is
        // itself a valid, legitimate `now_ms()` reading taken very early in
        // process lifetime, so it can't be used as an "unset" sentinel here).
        channels
            .editor_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("initial device-list broadcast was never sent within 5s of spawn()");

        // has_update is set synchronously before the recv_timeout returns above.
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
            now - ts < 5000,
            "last_update_ts ({ts}) is not within 5s of now_ms() ({now}) — wrong time base"
        );

        // Request shutdown so the non-macOS polling thread exits promptly;
        // macOS uses CFRunLoop and is cleaned up by the OS on process exit.
        channels.shutdown.store(true, Ordering::Release);
    }
}
