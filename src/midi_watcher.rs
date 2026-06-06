/// MIDI device hot-plug watcher.
///
/// On macOS this uses native CoreMIDI notification callbacks via
/// `coremidi::Client::new_with_notifications` + a CFRunLoop on a dedicated
/// thread — zero polling.  On non-macOS (Linux / Windows) it falls back to
/// periodic polling via `midir::MidiOutput`.
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use crossbeam_channel::{Receiver, Sender, bounded};
#[cfg(not(target_os = "macos"))]
use crossbeam_channel::tick;
#[cfg(not(target_os = "macos"))]
use std::time::Duration;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(not(target_os = "macos"))]
use std::sync::atomic::Ordering;

/// Minimum interval between device-list broadcasts — rate-limits flurries of
/// CoreMIDI notifications during USB hub plug/unplug.
const BROADCAST_COOLDOWN_MS: u64 = 300;

/// Polling interval for the non-macOS fallback.
#[cfg(not(target_os = "macos"))]
const POLL_INTERVAL_SECS: u64 = 2;

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
}

/// Spawns a background device watcher and returns two receivers (editor +
/// worker) that each receive updated device-name lists when the MIDI port
/// topology changes.
pub fn spawn() -> MidiWatcherChannels {
    let (ed_tx, ed_rx) = bounded::<Vec<String>>(16);
    let (wk_tx, wk_rx) = bounded::<Vec<String>>(16);
    let shutdown = Arc::new(AtomicBool::new(false));

    #[cfg(target_os = "macos")]
    spawn_macos(ed_tx, wk_tx);

    #[cfg(not(target_os = "macos"))]
    spawn_polling(ed_tx, wk_tx, shutdown.clone());

    MidiWatcherChannels { editor_rx: ed_rx, worker_rx: wk_rx, shutdown }
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
fn spawn_macos(ed_tx: Sender<Vec<String>>, wk_tx: Sender<Vec<String>>) {
    if let Err(e) = std::thread::Builder::new()
        .name("ethertap-midi-watch".into())
        .spawn(move || {
            use core_foundation::runloop::CFRunLoop;
            use coremidi::{Client, Notification};

            // Clone Senders before the move closure so we can also send
            // the initial device list before entering the run loop.
            let (ed_tx_cb, wk_tx_cb) = (ed_tx.clone(), wk_tx.clone());
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
                    // a single physical plug/unplug (USB hub topology).
                    static LAST_MS: OnceLock<AtomicU64> = OnceLock::new();
                    {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis() as u64);
                        let last = LAST_MS.get_or_init(|| AtomicU64::new(0));
                        let prev = last.load(AtomicOrdering::Relaxed);
                        if now_ms.saturating_sub(prev) < BROADCAST_COOLDOWN_MS {
                            return;
                        }
                        last.store(now_ms, AtomicOrdering::Relaxed);
                    }

                    let devices = enumerate_devices();
                    let _ = ed_tx_cb.try_send(devices.clone());
                    let _ = wk_tx_cb.try_send(devices);
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
        Ok(out) => out.ports()
            .iter()
            .filter_map(|p| out.port_name(p).ok())
            .filter(|n| n != "EtherTap MIDI Clock")
            .collect(),
        Err(e) => {
            log::warn!("[EtherTap] MIDI port scan failed: {e} \
                        (is ALSA/JACK available?)");
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_polling(ed_tx: Sender<Vec<String>>, wk_tx: Sender<Vec<String>>, shutdown: Arc<AtomicBool>) {
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

            let scan_timer = tick(Duration::from_secs(POLL_INTERVAL_SECS));
            loop {
                let _ = scan_timer.recv();
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let devices = scan_ports();
                let _ = ed_tx.try_send(devices.clone());
                let _ = wk_tx.try_send(devices);
            }
        })
    {
        log::error!("[EtherTap] failed to spawn MIDI poll thread: {e}");
    }
}
