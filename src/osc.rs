/// OSC message construction and X32-specific math.
///
/// BPM scaling mirrors the logic in X32Tap.c:
///   beat_ms = 60_000 / bpm
///   value   = beat_ms / 3000.0   (clamped 0.0 – 1.0)
///
/// OSC addresses used on the X32 / M32:
///   /fx/{slot}/type      – query which effect occupies a slot (returns i32)
///   /fx/{slot}/par/02    – set normalised delay time (float)
///   /fxrtn/{slot}/mix/on – mute/unmute the FX return (i32: 0=off, 1=on)
///   /info                – heartbeat / connectivity probe
use rosc::{encoder, OscMessage, OscPacket, OscType};

/// X32 effect-type ID for the Stereo Delay (from X32Tap.c: `#define DLY 10`).
pub const DLY_TYPE_ID: i32 = 10;

/// Maximum delay ceiling in milliseconds (X32Tap.c reference).
const MAX_DELAY_MS: f64 = 3_000.0;

// ─── BPM maths ─────────────────────────────────────────────────────────────

/// Convert host BPM to the X32's normalised delay-time float (0.0 – 1.0).
///
/// Derivation:
///   beat_ms = 60_000 / bpm
///   f       = beat_ms / 3000     (X32Tap.c ceiling)
///
/// # Examples
/// ```
/// assert!((ethertap::osc::bpm_to_float(120.0) - 0.1667).abs() < 0.001);
/// assert_eq!(ethertap::osc::bpm_to_float(20.0), 1.0);   // ceiling
/// ```
pub fn bpm_to_float(bpm: f64) -> f32 {
    debug_assert!(bpm > 0.0, "BPM must be positive");
    let beat_ms = 60_000.0 / bpm;
    (beat_ms / MAX_DELAY_MS).clamp(0.0, 1.0) as f32
}

// ─── OSC packet builders ────────────────────────────────────────────────────

/// `/fx/{slot}/par/02 <value>` — set normalised delay time.
pub fn set_fx_delay(slot: u8, value: f32) -> Vec<u8> {
    msg(format!("/fx/{slot}/par/02"), vec![OscType::Float(value)])
}

/// `/fx/{slot}/type` — query which effect occupies `slot`.
pub fn query_fx_type(slot: u8) -> Vec<u8> {
    msg(format!("/fx/{slot}/type"), vec![])
}

/// `/fxrtn/{slot}/mix/on <0|1>` — mute (0) or unmute (1) the FX return.
pub fn set_fxrtn_mute(slot: u8, muted: bool) -> Vec<u8> {
    msg(
        format!("/fxrtn/{slot}/mix/on"),
        vec![OscType::Int(if muted { 0 } else { 1 })],
    )
}

/// `/info` — heartbeat probe; X32 responds with console metadata.
pub fn heartbeat() -> Vec<u8> {
    msg("/info".to_owned(), vec![])
}

/// `/fx/{slot}/par/02` with **no arguments** — ask the X32 for the current
/// normalised delay time.  The console responds with the float value, which
/// can be decoded with [`float_to_bpm`].
pub fn query_fx_delay(slot: u8) -> Vec<u8> {
    msg(format!("/fx/{slot}/par/02"), vec![])
}

/// Convert an X32 normalised delay-time float (0.0–1.0) back to BPM.
///
/// Inverse of [`bpm_to_float`]:
///   `f = (60_000 / bpm) / 3000 = 20 / bpm`  ⟹  `bpm = 20 / f`
///
/// Returns `0.0` when `f ≤ 0` (no data received yet).
pub fn float_to_bpm(f: f32) -> f64 {
    if f <= 0.0 {
        return 0.0;
    }
    20.0 / f as f64
}

// ─── Internal helper ────────────────────────────────────────────────────────

fn msg(addr: String, args: Vec<OscType>) -> Vec<u8> {
    let packet = OscPacket::Message(OscMessage { addr, args });
    encoder::encode(&packet).expect("OSC encode is infallible for well-formed messages")
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bpm_scaling_120() {
        // 120 BPM → 500 ms → 500 / 3000 ≈ 0.1667
        let f = bpm_to_float(120.0);
        assert!((f - 0.1667_f32).abs() < 0.001, "120 BPM → {f}, expected ~0.1667");
    }

    #[test]
    fn bpm_scaling_60() {
        // 60 BPM → 1000 ms → 1000 / 3000 ≈ 0.3333
        let f = bpm_to_float(60.0);
        assert!((f - 0.3333_f32).abs() < 0.001, "60 BPM → {f}, expected ~0.3333");
    }

    #[test]
    fn bpm_scaling_ceiling() {
        // 20 BPM → 3000 ms → exactly 1.0
        assert_eq!(bpm_to_float(20.0), 1.0, "20 BPM should hit ceiling");
    }

    #[test]
    fn bpm_scaling_clamp_below_20() {
        // Anything slower than 20 BPM must clamp to 1.0
        assert_eq!(bpm_to_float(10.0), 1.0, "10 BPM must clamp to 1.0");
    }

    #[test]
    fn bpm_scaling_never_negative() {
        let f = bpm_to_float(9_999.0);
        assert!(f >= 0.0, "result must not be negative");
        assert!(f <= 1.0, "result must not exceed 1.0");
    }

    #[test]
    fn set_fx_delay_is_valid_osc() {
        let bytes = set_fx_delay(3, 0.25);
        // Decode and verify
        let packet = rosc::decoder::decode_udp(&bytes).expect("should decode");
        if let rosc::OscPacket::Message(m) = packet.1 {
            assert_eq!(m.addr, "/fx/3/par/02");
            assert_eq!(m.args, vec![rosc::OscType::Float(0.25)]);
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn heartbeat_is_valid_osc() {
        let bytes = heartbeat();
        let packet = rosc::decoder::decode_udp(&bytes).expect("should decode");
        if let rosc::OscPacket::Message(m) = packet.1 {
            assert_eq!(m.addr, "/info");
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn float_to_bpm_round_trip() {
        // Round-trip: bpm_to_float then float_to_bpm should recover the BPM
        for bpm in [60.0_f64, 90.0, 120.0, 140.0, 180.0] {
            let f = bpm_to_float(bpm);
            let recovered = float_to_bpm(f);
            assert!((recovered - bpm).abs() < 0.1, "bpm={bpm} → f={f} → {recovered}");
        }
    }

    #[test]
    fn float_to_bpm_no_data() {
        // f=0 (no data) should return 0.0 sentinel, not panic
        assert_eq!(float_to_bpm(0.0), 0.0);
    }

    #[test]
    fn query_fx_delay_is_get_message() {
        // A query (no args) should decode to a message with empty args
        let bytes = query_fx_delay(2);
        let packet = rosc::decoder::decode_udp(&bytes).expect("should decode");
        if let rosc::OscPacket::Message(m) = packet.1 {
            assert_eq!(m.addr, "/fx/2/par/02");
            assert!(m.args.is_empty(), "query must have no args");
        } else {
            panic!("expected a message");
        }
    }
}
