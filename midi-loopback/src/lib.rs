//! In-process software MIDI loopback: a process-global named-port registry.
//!
//! A "port" is a named, bounded `crossbeam_channel::bounded::<Vec<u8>>` queue.
//! One side `register`s a port (and owns the [`Receiver`] to drain incoming
//! messages); any number of other sides `connect` to that name to obtain a
//! [`Sender`] and push messages into it. This mirrors the connect-by-name
//! semantics of real MIDI ports (`phys_out`/`phys_in`) without any OS MIDI
//! driver, so tests and cross-platform code paths can exchange MIDI bytes
//! in-process.

use std::collections::HashMap;
use std::sync::OnceLock;

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;

/// Default channel capacity for registered ports.
pub const DEFAULT_CAPACITY: usize = 64;

/// Errors returned by registry operations.
#[derive(Debug, PartialEq, Eq)]
pub enum LoopbackError {
    /// `register` was called with a name that is already registered.
    NameTaken,
    /// `connect` (or `unregister`) was called with a name that has no
    /// registered port.
    NotFound,
}

/// A registered loopback port. Dropping this unregisters the port,
/// freeing the name for reuse.
pub struct LoopbackPort {
    name: String,
    rx: Receiver<Vec<u8>>,
}

impl LoopbackPort {
    /// Receive a message previously sent via a [`Sender`] obtained from
    /// [`connect`], without blocking.
    pub fn try_recv(&self) -> Result<Vec<u8>, crossbeam_channel::TryRecvError> {
        self.rx.try_recv()
    }

    /// The name this port was registered under.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for LoopbackPort {
    fn drop(&mut self) {
        unregister(&self.name);
    }
}

fn registry() -> &'static Mutex<HashMap<String, Sender<Vec<u8>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Sender<Vec<u8>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a new named port with the given channel capacity.
///
/// Returns [`LoopbackError::NameTaken`] if `name` is already registered.
pub fn register(name: &str, capacity: usize) -> Result<LoopbackPort, LoopbackError> {
    let mut reg = registry().lock();
    if reg.contains_key(name) {
        return Err(LoopbackError::NameTaken);
    }
    let (tx, rx) = bounded(capacity);
    reg.insert(name.to_string(), tx);
    Ok(LoopbackPort {
        name: name.to_string(),
        rx,
    })
}

/// Connect to an existing named port, returning a [`Sender`] that can be
/// used to push messages into it.
///
/// Returns [`LoopbackError::NotFound`] if no port is registered under `name`.
pub fn connect(name: &str) -> Result<Sender<Vec<u8>>, LoopbackError> {
    let reg = registry().lock();
    reg.get(name).cloned().ok_or(LoopbackError::NotFound)
}

/// Send a message to a named port, connecting first.
///
/// Returns [`LoopbackError::NotFound`] if no port is registered under `name`,
/// or `Err` propagating a send failure (e.g. the port's receiver was dropped
/// or the channel is full) wrapped as [`LoopbackError::NotFound`] — callers
/// needing finer-grained send errors should use [`connect`] directly.
pub fn send(name: &str, message: Vec<u8>) -> Result<(), LoopbackError> {
    let tx = connect(name)?;
    tx.try_send(message).map_err(|_| LoopbackError::NotFound)
}

/// Remove a named port from the registry, if present.
///
/// This is also called automatically when a [`LoopbackPort`] is dropped.
pub fn unregister(name: &str) {
    registry().lock().remove(name);
}

/// Return the names of all currently-registered ports.
///
/// Used by port-presence checks (e.g. `handle_port_scan`'s periodic-scan
/// timer) so a connected loopback port is never reported as "disappeared"
/// just because it's invisible to midir's hardware port enumeration.
pub fn registered_names() -> Vec<String> {
    registry().lock().keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test uses a unique name to avoid cross-test collisions, since
    /// the registry is process-global.
    fn unique_name(tag: &str) -> String {
        format!(
            "test-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        )
    }

    #[test]
    fn register_creates_port() {
        let name = unique_name("register");
        let port = register(&name, DEFAULT_CAPACITY).expect("register should succeed");
        assert_eq!(port.name(), name);
        unregister(&name);
    }

    #[test]
    fn connect_and_send_recv_roundtrip() {
        let name = unique_name("roundtrip");
        let port = register(&name, DEFAULT_CAPACITY).expect("register should succeed");

        let tx = connect(&name).expect("connect should find the registered port");
        tx.try_send(vec![0xF8]).expect("send should succeed");

        let received = port.try_recv().expect("recv should yield the sent message");
        assert_eq!(received, vec![0xF8]);

        unregister(&name);
    }

    #[test]
    fn send_helper_roundtrip() {
        let name = unique_name("send-helper");
        let port = register(&name, DEFAULT_CAPACITY).expect("register should succeed");

        send(&name, vec![0x90, 0x40, 0x7F]).expect("send should succeed");

        let received = port.try_recv().expect("recv should yield the sent message");
        assert_eq!(received, vec![0x90, 0x40, 0x7F]);

        unregister(&name);
    }

    #[test]
    fn unregister_frees_name_for_reuse() {
        let name = unique_name("unregister");
        let port = register(&name, DEFAULT_CAPACITY).expect("first register should succeed");
        unregister(&name);
        drop(port);

        // Name is free again — a second register with the same name succeeds.
        let port2 = register(&name, DEFAULT_CAPACITY).expect("re-register should succeed");
        unregister(port2.name());
    }

    #[test]
    fn dropping_port_unregisters_it() {
        let name = unique_name("drop-unregisters");
        let port = register(&name, DEFAULT_CAPACITY).expect("register should succeed");
        drop(port);

        // connect should now fail — the name is gone.
        assert_eq!(connect(&name).err(), Some(LoopbackError::NotFound));
    }

    #[test]
    fn register_name_collision_is_rejected() {
        let name = unique_name("collision");
        let port = register(&name, DEFAULT_CAPACITY).expect("first register should succeed");

        let second = register(&name, DEFAULT_CAPACITY);
        assert_eq!(second.err(), Some(LoopbackError::NameTaken));

        unregister(&name);
        drop(port);
    }

    #[test]
    fn connect_to_unknown_name_fails() {
        let name = unique_name("unknown");
        assert_eq!(connect(&name).err(), Some(LoopbackError::NotFound));
    }

    #[test]
    fn registered_names_includes_registered_port_and_excludes_after_unregister() {
        let name = unique_name("registered-names");
        let port = register(&name, DEFAULT_CAPACITY).expect("register should succeed");

        assert!(
            registered_names().contains(&name),
            "registered_names should include a freshly-registered port"
        );

        drop(port);

        assert!(
            !registered_names().contains(&name),
            "registered_names should not include a port after it's dropped/unregistered"
        );
    }
}
