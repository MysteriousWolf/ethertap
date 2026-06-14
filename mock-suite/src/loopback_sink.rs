//! MIDI clock sink backed by the in-process [`midi_loopback`] registry — a
//! cross-platform sibling to [`crate::clock_sink::MidiClockSink`] (which
//! requires a real OS virtual MIDI port and is therefore `cfg(unix)`).
//!
//! `LoopbackClockSink` registers a named loopback port and drains it on a
//! background polling thread, feeding received bytes into the same
//! [`crate::sink_state::SinkState`] accumulation logic that the OS-virtual-port
//! sink uses. No `midir`/unix dependency — compiles and runs on Windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use midi_loopback::{register, LoopbackPort};
use parking_lot::Mutex;

use crate::sink_state::SinkState;
use crate::SinkStats;

/// How often the drain thread polls the loopback port for new messages.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

pub struct LoopbackClockSink {
    state: Arc<Mutex<SinkState>>,
    running: Arc<AtomicBool>,
    drain_thread: Option<JoinHandle<()>>,
    port_name: String,
}

impl LoopbackClockSink {
    /// Register a loopback port under `port_name` and start draining it.
    ///
    /// Returns an error if `port_name` is already registered (see
    /// [`midi_loopback::LoopbackError`]).
    pub fn start_named(port_name: &str) -> Result<Self, String> {
        let port: LoopbackPort =
            register(port_name, midi_loopback::DEFAULT_CAPACITY).map_err(|e| format!("{e:?}"))?;

        let state = Arc::new(Mutex::new(SinkState::default()));
        let running = Arc::new(AtomicBool::new(true));

        let drain_state = state.clone();
        let drain_running = running.clone();
        let drain_thread = std::thread::spawn(move || {
            // `port` is moved into the thread and dropped on exit, which
            // unregisters the name from the registry.
            while drain_running.load(Ordering::Relaxed) {
                match port.try_recv() {
                    Ok(message) => drain_state.lock().on_message(&message),
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }
        });

        Ok(Self {
            state,
            running,
            drain_thread: Some(drain_thread),
            port_name: port_name.to_string(),
        })
    }

    /// The registered port's name.
    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Stop draining and unregister the port. Idempotent.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.drain_thread.take() {
            let _ = handle.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.drain_thread.is_some()
    }

    /// Total 0xF8 clock bytes received since start.
    pub fn total_clocks(&self) -> u64 {
        self.state.lock().total_clocks()
    }

    /// Compute interval/jitter statistics over the current window. Returns
    /// `None` until at least two clocks have arrived.
    pub fn stats(&self) -> Option<SinkStats> {
        self.state.lock().stats()
    }
}

impl Drop for LoopbackClockSink {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test uses a unique name to avoid cross-test collisions, since
    /// the `midi_loopback` registry is process-global.
    fn unique_name(tag: &str) -> String {
        format!(
            "loopback-sink-test-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        )
    }

    #[test]
    fn counts_clock_bytes_sent_to_registered_port() {
        let name = unique_name("counts");
        let sink = LoopbackClockSink::start_named(&name).expect("start_named should succeed");
        assert!(sink.is_running());
        assert_eq!(sink.port_name(), name);
        assert_eq!(sink.total_clocks(), 0);

        // Send a handful of 0xF8 clock bytes from the "worker" side.
        for _ in 0..5 {
            midi_loopback::send(&name, vec![0xF8]).expect("send should succeed");
        }

        // Poll until the drain thread has consumed all messages.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while sink.total_clocks() < 5 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(sink.total_clocks(), 5);
    }

    #[test]
    fn stats_none_until_two_clocks_received() {
        let name = unique_name("stats-none");
        let sink = LoopbackClockSink::start_named(&name).expect("start_named should succeed");
        assert!(sink.stats().is_none());
    }
}
