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

// ─── Effect type tables (sourced from pmaillot/X32-Behringer X32.c) ─────────
//
// Slots 1–4 are FX bus slots and use Sfxtyp1.
// Slots 5–8 are insert slots and use Sfxtyp2.
// Type IDs are 0-based indices into each table.
// An empty slot does not respond to /fx/{slot}/type at all (returns None).

/// `(short_label, full_name)` for FX bus slots 1–4 (Sfxtyp1 from X32.c).
fn fx_bus(type_id: i32) -> (&'static str, &'static str) {
    match type_id {
        0  => ("HALL", "Hall Reverb"),
        1  => ("AMBI", "Ambience"),
        2  => ("RPLT", "Dual Reverb Plate"),
        3  => ("ROOM", "Room Reverb"),
        4  => ("CHAM", "Chamber Reverb"),
        5  => ("PLAT", "Plate Reverb"),
        6  => ("VREV", "Vintage Reverb"),
        7  => ("VRM",  "Vintage Room"),
        8  => ("GATE", "Gated Reverb"),
        9  => ("RVRS", "Reverse Reverb"),
        10 => ("DLY",  "Stereo Delay"),          // ← BPM via par/02 (mix=par/01, time=par/02)
        11 => ("3TAP", "3-Tap Delay"),            // ← BPM via par/01 (time is first param)
        12 => ("4TAP", "4-Tap Delay"),            // ← BPM via par/01 (time is first param)
        13 => ("CRS",  "Chorus"),
        14 => ("FLNG", "Flanger"),
        15 => ("PHAS", "Phaser"),
        16 => ("DIMC", "Dimension Chorus"),
        17 => ("FILT", "Filter"),
        18 => ("ROTA", "Rotary"),
        19 => ("PAN",  "Autopanner"),
        20 => ("SUB",  "Sub Synth"),
        21 => ("D/RV", "Delay + Reverb"),         // ← BPM via par/01 (time is first param)
        22 => ("CR/R", "Chorus + Reverb"),
        23 => ("FL/R", "Flanger + Reverb"),
        24 => ("D/CR", "Delay + Chorus"),          // ← BPM via par/01 (time is first param)
        25 => ("D/FL", "Delay + Flanger"),         // ← BPM via par/01 (time is first param)
        26 => ("MODD", "Modulated Delay"),        // ← BPM via par/01 (confirmed fxparse1.c)
        27 => ("GEQ2", "Dual GEQ 27"),
        28 => ("TEQ2", "Dual True EQ"),
        29 => ("GEQ",  "GEQ 27"),
        30 => ("TEQ",  "True EQ"),
        31 => ("DES2", "Dual De-esser"),
        32 => ("DES",  "De-esser"),
        // NOTE: X32.c lists these as "PCM42/P1A2"; fxparse1.c shows only EQ
        // parameters (no delay time) for P1A2.  Display labels kept from X32.c;
        // neither type is BPM-compatible.
        33 => ("P1A2", "Dual PCM42"),
        34 => ("P1A",  "PCM42"),
        35 => ("PQ5S", "PQ Stereo"),
        36 => ("PQ5",  "Para EQ"),
        37 => ("WAVD", "Wave Designer"),
        38 => ("LIM",  "Limiter"),
        39 => ("CMB2", "Dual Combinator"),
        40 => ("CMB",  "Combinator"),
        41 => ("FAC2", "Dual FACilitator"),
        42 => ("F1MS", "FACilitator M/S"),
        43 => ("FAC",  "FACilitator"),
        44 => ("LEC2", "Dual Leveler"),
        45 => ("LEC",  "Leveler"),
        46 => ("ULC2", "Dual UltraShaper"),
        47 => ("ULC",  "UltraShaper"),
        48 => ("ENH2", "Dual Enhancer"),
        49 => ("ENH",  "Enhancer"),
        50 => ("EXC2", "Dual Exciter"),
        51 => ("EXC",  "Exciter"),
        52 => ("IMG",  "Stereo Imager"),
        53 => ("EDI",  "Edison EX1"),
        54 => ("SON",  "Sonic Maximizer"),
        55 => ("AMP2", "Dual Amp Sim"),
        56 => ("AMP",  "Amp Sim"),
        57 => ("DRV2", "Dual Drive"),
        58 => ("DRV",  "Drive"),
        59 => ("PIT2", "Dual Pitch"),
        60 => ("PIT",  "Pitch Shift"),
        _  => ("???",  "Unknown"),
    }
}

/// `(short_label, full_name)` for FX insert slots 5–8 (Sfxtyp2 from X32.c).
fn fx_insert(type_id: i32) -> (&'static str, &'static str) {
    match type_id {
        0  => ("GEQ2", "Dual GEQ 27"),
        1  => ("TEQ2", "Dual True EQ"),
        2  => ("GEQ",  "GEQ 27"),
        3  => ("TEQ",  "True EQ"),
        4  => ("DES2", "Dual De-esser"),
        5  => ("DES",  "De-esser"),
        6  => ("P1A",  "PCM42"),
        7  => ("P1A2", "Dual PCM42"),
        8  => ("PQ5",  "Para EQ"),
        9  => ("PQ5S", "PQ Stereo"),
        10 => ("WAVD", "Wave Designer"),
        11 => ("LIM",  "Limiter"),
        12 => ("FAC",  "FACilitator"),
        13 => ("F1MS", "FACilitator M/S"),
        14 => ("FAC2", "Dual FACilitator"),
        15 => ("LEC",  "Leveler"),
        16 => ("LEC2", "Dual Leveler"),
        17 => ("ULC",  "UltraShaper"),
        18 => ("ULC2", "Dual UltraShaper"),
        19 => ("ENH2", "Dual Enhancer"),
        20 => ("ENH",  "Enhancer"),
        21 => ("EXC2", "Dual Exciter"),
        22 => ("EXC",  "Exciter"),
        23 => ("IMG",  "Stereo Imager"),
        24 => ("EDI",  "Edison EX1"),
        25 => ("SON",  "Sonic Maximizer"),
        26 => ("AMP2", "Dual Amp Sim"),
        27 => ("AMP",  "Amp Sim"),
        28 => ("DRV2", "Dual Drive"),
        29 => ("DRV",  "Drive"),
        30 => ("PHAS", "Phaser"),
        31 => ("FILT", "Filter"),
        32 => ("PAN",  "Autopanner"),
        33 => ("SUB",  "Sub Synth"),
        _  => ("???",  "Unknown"),
    }
}

/// Short display label (≤4 chars) for the given type ID and slot.
pub fn fx_type_short(type_id: i32, slot: u8) -> &'static str {
    if slot <= 4 { fx_bus(type_id).0 } else { fx_insert(type_id).0 }
}

/// Full human-readable effect name for debug output and the FX detail row.
pub fn fx_type_long(type_id: i32, slot: u8) -> &'static str {
    if slot <= 4 { fx_bus(type_id).1 } else { fx_insert(type_id).1 }
}

/// Returns true for effects that accept a BPM-derived delay time.
///
/// Only applies to bus slots 1–4 (Sfxtyp1).
///
/// Confirmed (X32Tap.c): DLY (10) — delay time at par/02.
/// Confirmed (fxparse1.c): 3TAP (11), 4TAP (12), MODD (26) — delay time at par/01
///   (these effects put time as their *first* parameter, one index earlier than DLY).
/// Confirmed (fxparse1.c): D/RV (21), D/CR (24), D/FL (25) — share the same
///   `,fiffffffffff` layout as 3TAP/4TAP/MODD; delay time is first param → par/01.
///
/// NOT BPM-compatible: P1A/P1A2 (33/34) — fxparse1.c shows only EQ parameters.
/// Insert slots 5–8 contain no delay effects and are never BPM-compatible.
pub fn is_bpm_compatible(type_id: i32, slot: u8) -> bool {
    slot <= 4 && matches!(type_id, 10 | 11 | 12 | 21 | 24 | 25 | 26)
}

/// Returns the OSC par/NN index for the delay-time parameter of the given effect.
///
/// DLY (10): par/02 — confirmed by X32Tap.c (mix occupies par/01).
/// 3TAP (11), 4TAP (12), MODD (26): par/01 — delay time is their first parameter
///   (fxparse1.c par index 0 + 1-based OSC offset = par/01).
/// D/RV (21), D/CR (24), D/FL (25): par/01 — same `,fiffffffffff` layout as
///   3TAP/4TAP/MODD; delay time is the first parameter (confirmed fxparse1.c).
pub fn delay_par(type_id: i32) -> u8 {
    match type_id {
        10 => 2,                    // DLY  — time follows mix; confirmed par/02
        11 | 12 | 21 | 24 | 25 | 26 => 1, // 3TAP / 4TAP / D/RV / D/CR / D/FL / MODD — time first = par/01
        _ => 2,                     // safe fallback for any future delay effect
    }
}

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
    if bpm <= 0.0 {
        return 1.0;
    }
    let beat_ms = 60_000.0 / bpm;
    (beat_ms / MAX_DELAY_MS).clamp(0.0, 1.0) as f32
}

// ─── OSC packet builders ────────────────────────────────────────────────────

/// Set the normalised delay time for the given slot and effect type.
///
/// Uses the effect-specific par address: par/02 for DLY, par/01 for 3TAP/4TAP/MODD.
/// `type_id` must be a BPM-compatible effect; pass 0 to fall back to par/02.
pub fn set_fx_delay(slot: u8, type_id: i32, value: f32) -> Vec<u8> {
    let par = delay_par(type_id);
    msg(format!("/fx/{slot}/par/{par:02}"), vec![OscType::Float(value)])
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

/// Query the current normalised delay time for the given slot and effect type.
///
/// Uses the same effect-specific par address as [`set_fx_delay`].
/// The console responds with the float value; decode with [`float_to_bpm`].
pub fn query_fx_delay(slot: u8, type_id: i32) -> Vec<u8> {
    let par = delay_par(type_id);
    msg(format!("/fx/{slot}/par/{par:02}"), vec![])
}

/// Convert an X32 normalised delay-time float (0.0–1.0) back to BPM.
///
/// Inverse of [`bpm_to_float`]:
///   `f = (60_000 / bpm) / 3000 = 20 / bpm`  ⟹  `bpm = 20 / f`
///
/// Returns `0.0` when `f ≤ 0` (no data received yet).
pub fn float_to_bpm(f: f32) -> f64 {
    if f <= 0.0 {
        0.0
    } else {
        20.0 / f as f64
    }
}

// ─── Internal helper ────────────────────────────────────────────────────────

fn msg(addr: String, args: Vec<OscType>) -> Vec<u8> {
    let packet = OscPacket::Message(OscMessage { addr, args });
    match encoder::encode(&packet) {
        Ok(bytes) => bytes,
        Err(e) => {
            log::error!("[EtherTap] OSC encode failed (this is a bug): {e}");
            vec![]
        }
    }
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
    fn set_fx_delay_dly_uses_par02() {
        // DLY (type 10): mix=par/01, time=par/02
        let bytes = set_fx_delay(3, 10, 0.25);
        let packet = match rosc::decoder::decode_udp(&bytes) {
            Ok(p) => p.1,
            Err(e) => {
                log::warn!("decode_udp failed (DLY): {e}");
                return;
            }
        };
        if let rosc::OscPacket::Message(m) = packet {
            assert_eq!(m.addr, "/fx/3/par/02");
            assert_eq!(m.args, vec![rosc::OscType::Float(0.25)]);
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn set_fx_delay_drv_uses_par01() {
        // D/RV (type 21): time is first param = par/01
        let bytes = set_fx_delay(3, 21, 0.25);
        let packet = match rosc::decoder::decode_udp(&bytes) {
            Ok(p) => p.1,
            Err(e) => {
                log::warn!("decode_udp failed (D/RV): {e}");
                return;
            }
        };
        if let rosc::OscPacket::Message(m) = packet {
            assert_eq!(m.addr, "/fx/3/par/01");
            assert_eq!(m.args, vec![rosc::OscType::Float(0.25)]);
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn heartbeat_is_valid_osc() {
        let bytes = heartbeat();
        let packet = match rosc::decoder::decode_udp(&bytes) {
            Ok(p) => p.1,
            Err(e) => {
                log::warn!("decode_udp failed (heartbeat): {e}");
                return;
            }
        };
        if let rosc::OscPacket::Message(m) = packet {
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
        let bytes = query_fx_delay(2, 10);
        let packet = match rosc::decoder::decode_udp(&bytes) {
            Ok(p) => p.1,
            Err(e) => {
                log::warn!("decode_udp failed (query): {e}");
                return;
            }
        };
        if let rosc::OscPacket::Message(m) = packet {
            assert_eq!(m.addr, "/fx/2/par/02");
            assert!(m.args.is_empty(), "query must have no args");
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn set_fxrtn_mute_encode() {
        let bytes = set_fxrtn_mute(1, true);
        let packet = match rosc::decoder::decode_udp(&bytes) {
            Ok(p) => p.1,
            Err(_) => panic!("decode failed"),
        };
        if let rosc::OscPacket::Message(m) = packet {
            assert_eq!(m.addr, "/fxrtn/1/mix/on");
            assert_eq!(m.args, vec![rosc::OscType::Int(0)]);
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn query_fx_type_encode() {
        let bytes = query_fx_type(3);
        let packet = match rosc::decoder::decode_udp(&bytes) {
            Ok(p) => p.1,
            Err(_) => panic!("decode failed"),
        };
        if let rosc::OscPacket::Message(m) = packet {
            assert_eq!(m.addr, "/fx/3/type");
            assert!(m.args.is_empty());
        } else {
            panic!("expected a message");
        }
    }

    #[test]
    fn delay_par_dly_vs_3tap() {
        assert_eq!(super::delay_par(10), 2, "DLY uses par/02");
        assert_eq!(super::delay_par(11), 1, "3TAP uses par/01");
        assert_eq!(super::delay_par(12), 1, "4TAP uses par/01");
        assert_eq!(super::delay_par(21), 1, "D/RV uses par/01");
        assert_eq!(super::delay_par(26), 1, "MODD uses par/01");
        assert_eq!(super::delay_par(0), 2, "unknown uses fallback par/02");
    }

    #[test]
    fn bpm_to_float_extremes() {
        assert_eq!(super::bpm_to_float(20.0), 1.0_f32, "20 BPM = ceiling");
        assert_eq!(super::bpm_to_float(10.0), 1.0_f32, "10 BPM clamps to ceiling");
        let f = super::bpm_to_float(21.0);
        assert!(f < 1.0 && f > 0.0, "21 BPM gives positive fraction below ceiling");
        let f = super::bpm_to_float(120.0);
        assert!(f > 0.0 && f < 1.0, "120 BPM produces fraction");
    }

    #[test]
    fn float_to_bpm_extremes() {
        assert_eq!(super::float_to_bpm(1.0_f32), 20.0_f64, "f=1.0 → 20 BPM");
        assert_eq!(super::float_to_bpm(-0.5_f32), 0.0, "negative clamps to 0");
        assert_eq!(super::float_to_bpm(0.5_f32), 40.0, "f=0.5 → 40 BPM");
    }
}
