/// Mock X32/M32 mixer for standalone GUI testing.
///
/// Listens on `127.0.0.1:10023` and responds to the three OSC message types
/// the NetworkWorker sends:
///
/// | Incoming                          | Response                          |
/// |-----------------------------------|-----------------------------------|
/// | `/info`                           | `/info` + string (→ Connected)    |
/// | `/fx/{slot}/par/02`  (no args)    | same addr + float (mock BPM=120)  |
/// | `/fx/{slot}/type`    (any args)   | same addr + i32=10 (DLY)         |
/// | `/fxrtn/{slot}/mix/on` or SET msg | ignored                           |
use std::{net::UdpSocket, sync::Once, thread, time::Duration};

use rosc::{encoder, OscMessage, OscPacket, OscType};

static ONCE: Once = Once::new();

/// Mock BPM shown in the telemetry row.
const MOCK_BPM: f32 = 120.0;
/// Pre-computed delay float for 120 BPM: (60_000/120)/3000 ≈ 0.16667.
const MOCK_FLOAT: f32 = 20.0 / MOCK_BPM;

/// Spawn the mock mixer thread exactly once (safe to call from `Default`).
///
/// Blocks for 30 ms so the socket is bound before the `NetworkWorker` sends
/// its first `UpdateTarget` command.
pub fn start_once() {
    ONCE.call_once(|| {
        thread::Builder::new()
            .name("mock-x32".into())
            .spawn(run)
            .expect("spawn mock-x32 thread");
        // Give the mock time to bind before NetworkWorker tries to connect.
        thread::sleep(Duration::from_millis(30));
    });
}

fn run() {
    let socket = match UdpSocket::bind("127.0.0.1:10023") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[mock-x32] bind failed: {e}  (port already in use?)");
            return;
        }
    };
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();
    eprintln!("[mock-x32] ready on 127.0.0.1:10023  (mock BPM = {MOCK_BPM})");

    let mut buf = [0u8; 1024];
    loop {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            respond(&socket, &buf[..len], from);
        }
    }
}

fn respond(socket: &UdpSocket, data: &[u8], from: std::net::SocketAddr) {
    let Ok((_, OscPacket::Message(msg))) = rosc::decoder::decode_udp(data) else {
        return;
    };

    let args: Vec<OscType> = if msg.addr == "/info" {
        vec![OscType::String("mock-x32-standalone".to_owned())]
    } else if msg.addr.ends_with("/par/02") && msg.args.is_empty() {
        // Telemetry query: return float for MOCK_BPM
        vec![OscType::Float(MOCK_FLOAT)]
    } else if msg.addr.ends_with("/type") {
        // Audit query: report Stereo Delay (DLY = type 10) for every slot
        vec![OscType::Int(10)]
    } else {
        return; // SET commands and anything else are ignored
    };

    let reply = OscPacket::Message(OscMessage {
        addr: msg.addr,
        args,
    });
    if let Ok(bytes) = encoder::encode(&reply) {
        socket.send_to(&bytes, from).ok();
    }
}
