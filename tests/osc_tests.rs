//! Comprehensive OSC encoding/decoding tests.
//!
//! These tests are deterministic and do not require hardware.

use ethertap::osc::{self, bpm_to_float, float_to_bpm};
use rosc::OscType;

/// Wrapper to decode raw bytes to an OSC message for inspection.
fn decode_msg(bytes: &[u8]) -> Option<rosc::OscMessage> {
    match rosc::decoder::decode_udp(bytes) {
        Ok((_, rosc::OscPacket::Message(m))) => Some(m),
        _ => None,
    }
}

// ─── 1. Encode/Decode roundtrip for /fx/{n}/par/02 float ──────────────────────

#[test]
fn roundtrip_fx_delay_par02_dly() {
    // DLY (type 10) uses par/02 confirmed by X32Tap.c
    for slot in [1u8, 4] {
        for &value in &[0.0_f32, 0.5, 1.0, 0.1667] {
            let bytes = osc::set_fx_delay(slot, 10, value);
            let msg = decode_msg(&bytes).expect("must decode");
            assert_eq!(msg.addr, format!("/fx/{slot}/par/02"));
            assert_eq!(msg.args, vec![OscType::Float(value)]);
        }
    }
}

#[test]
fn roundtrip_set_fx_delay_all_par01_effects() {
    // All non-DLY BPM-compatible effects use par/01 (time is their first parameter).
    // 3TAP (11), 4TAP (12): confirmed fxparse1.c.
    // D/RV (21), D/CR (24), D/FL (25): same `,fiffffffffff` layout — time first.
    // MODD (26): confirmed fxparse1.c.
    let par01_effects = [
        (11, "3TAP"),
        (12, "4TAP"),
        (21, "D/RV"),
        (24, "D/CR"),
        (25, "D/FL"),
        (26, "MODD"),
    ];
    for (type_id, name) in par01_effects {
        let bytes = osc::set_fx_delay(2, type_id, 0.3);
        let msg = decode_msg(&bytes).expect("must decode");
        assert_eq!(
            msg.addr, "/fx/2/par/01",
            "{name} (type {type_id}) should use par/01"
        );
        assert_eq!(msg.args, vec![OscType::Float(0.3)], "{name}");
    }
}

// ─── 2. Encode/Decode roundtrip for /info response ────────────────────────────

#[test]
fn roundtrip_heartbeat() {
    let bytes = osc::heartbeat();
    let msg = decode_msg(&bytes).expect("must decode");
    assert_eq!(msg.addr, "/info");
    assert!(msg.args.is_empty(), "/info must have no args");
}

// ─── 3. Encode/Decode roundtrip for /fx/{n}/type query response ───────────────

#[test]
fn roundtrip_query_fx_type_all_slots() {
    for slot in 1..=8 {
        let bytes = osc::query_fx_type(slot);
        let msg = decode_msg(&bytes).expect("must decode");
        assert_eq!(msg.addr, format!("/fx/{slot}/type"));
        assert!(msg.args.is_empty(), "type query must have no args");
    }
}

// ─── Mute/unmute int encoding: wrong polarity would silently break X32 ─────────

#[test]
fn set_fxrtn_mute_int_value_polarity() {
    // X32 on/off: 0 = off (muted), 1 = on (unmuted). EtherTap inverts the
    // boolean: mute=true → 0, mute=false → 1. A swapped polarity here would
    // send "unmute" when the protocol expects "mute" — audio plays through a
    // Hard Reset instead of cutting.
    let muted = osc::set_fxrtn_mute(1, true);
    let m = decode_msg(&muted).expect("must decode");
    assert_eq!(m.args, vec![OscType::Int(0)], "mute=true must send 0 (off)");

    let unmuted = osc::set_fxrtn_mute(1, false);
    let u = decode_msg(&unmuted).expect("must decode");
    assert_eq!(u.args, vec![OscType::Int(1)], "mute=false must send 1 (on)");
}

// ─── Malformed packet handling – rejection of structurally invalid inputs ──────

// ─── Additional roundtrip consistency tests ────────────────────────────────────

#[test]
fn roundtrip_set_fxrtn_mute_all_slots() {
    // Polarity: mute=true → 0 (off), mute=false → 1 (on). Hardcoded, not mirrored.
    // Covers slots 2-8; slot 1 is already pinned by set_fxrtn_mute_int_value_polarity.
    for slot in 2..=8 {
        let muted = osc::set_fxrtn_mute(slot, true);
        let msg = decode_msg(&muted).expect("must decode");
        assert_eq!(msg.addr, format!("/fxrtn/{slot}/mix/on"));
        assert_eq!(
            msg.args,
            vec![OscType::Int(0)],
            "slot {slot} mute=true must send 0"
        );

        let unmuted = osc::set_fxrtn_mute(slot, false);
        let msg = decode_msg(&unmuted).expect("must decode");
        assert_eq!(msg.addr, format!("/fxrtn/{slot}/mix/on"));
        assert_eq!(
            msg.args,
            vec![OscType::Int(1)],
            "slot {slot} mute=false must send 1"
        );
    }
}

#[test]
fn roundtrip_query_fx_delay_all_effects() {
    // par numbers are protocol constants from the OSC quick reference.
    // Hardcoded here so a wrong delay_par() mapping makes both sides wrong simultaneously.
    let expected_pars: &[(i32, u8)] = &[
        (10, 2), // DLY       — time=par/02  (confirmed X32Tap.c)
        (11, 1), // 3TAP      — time=par/01
        (12, 1), // 4TAP      — time=par/01
        (21, 1), // D/RV      — time=par/01
        (24, 1), // D/CR      — time=par/01
        (25, 1), // D/FL      — time=par/01
        (26, 1), // MODD      — time=par/01
    ];
    for slot in 1u8..=4 {
        for &(type_id, expected_par) in expected_pars {
            let bytes = osc::query_fx_delay(slot, type_id);
            let msg = decode_msg(&bytes).expect("must decode");
            assert_eq!(
                msg.addr,
                format!("/fx/{slot}/par/{expected_par:02}"),
                "type {type_id} should use par/{expected_par:02}"
            );
            assert!(msg.args.is_empty(), "query must have no args");
        }
    }
}

#[test]
fn float_to_bpm_zero_is_unset_sentinel() {
    // 0.0 means "no readback data yet" — the sentinel used to detect a mixer
    // that hasn't responded. Callers must gate on this before comparing BPM.
    assert_eq!(
        float_to_bpm(0.0),
        0.0,
        "float 0.0 must return sentinel 0.0 (not a divide-by-zero)"
    );
}

#[test]
fn float_to_bpm_negative_is_sentinel() {
    // Negative values (e.g. corrupted mixer data) must return 0.0, not a
    // negative BPM. The implementation guards with `if f <= 0.0`.
    assert_eq!(
        float_to_bpm(-0.5),
        0.0,
        "negative float must return sentinel 0.0"
    );
    assert_eq!(
        float_to_bpm(-1.0),
        0.0,
        "negative float must return sentinel 0.0"
    );
}

#[test]
fn bpm_to_float_protocol_range() {
    // Protocol range: BPM=20 maps to par=1.0 (maximum delay = slowest tempo
    // the X32 can represent); BPM=300 maps to par≈0.067. Values outside 20-300
    // still produce a valid 0..=1 float — they clamp rather than panic.
    let f20 = bpm_to_float(20.0);
    assert!(
        (f20 - 1.0_f32).abs() < 0.001,
        "BPM 20 should produce par≈1.0, got {f20}"
    );
    let f300 = bpm_to_float(300.0);
    assert!(
        f300 > 0.0 && f300 < 0.1,
        "BPM 300 should produce a small positive par, got {f300}"
    );
    // Below the minimum representable BPM: clamps to 1.0.
    let f_low = bpm_to_float(1.0);
    assert_eq!(
        f_low, 1.0_f32,
        "BPM below protocol minimum should clamp to 1.0"
    );
    // Well above maximum: near-zero par (mixer receives near-zero delay time).
    let f_high = bpm_to_float(10_000.0);
    assert!(
        (0.0..=0.01).contains(&f_high),
        "BPM well above protocol max should produce near-zero par, got {f_high}"
    );
}

#[test]
fn bpm_float_roundtrip_accuracy() {
    // Verify roundtrip accuracy across BPM range
    let bpms = [30.0_f64, 60.0, 90.0, 120.0, 140.0, 180.0, 200.0, 240.0];
    for bpm in bpms {
        let f = bpm_to_float(bpm);
        let recovered = float_to_bpm(f);
        let error = (recovered - bpm).abs();
        assert!(
            error < 0.1,
            "BPM roundtrip error too high: {bpm} -> {f} -> {recovered}, error={error}"
        );
    }
}

// ─── fx_type_short / fx_type_long coverage ────────────────────────────────────
// These tests call fx_type_short/long across all valid type IDs and both slot
// ranges (bus slots 1–4, insert slots 5–8) to exercise every match arm in the
// fx_bus and fx_insert lookup tables.

#[test]
fn fx_type_short_bus_slot_all_type_ids() {
    // Bus slots 1–4 use the Sfxtyp1 table (61 known types + unknown fallback).
    for type_id in 0i32..=60 {
        let s = osc::fx_type_short(type_id, 1);
        assert!(
            !s.is_empty(),
            "type_id {type_id} should have a non-empty short label"
        );
        assert!(
            s.len() <= 4,
            "short label for type_id {type_id} should be ≤4 chars: {s:?}"
        );
    }
    // Unknown type_id must fall back to "???"
    assert_eq!(osc::fx_type_short(999, 1), "???");
    assert_eq!(osc::fx_type_short(-2, 1), "???");
    // Spot-check BPM-compatible types — wrong short labels would silently produce
    // confusing UI text without failing any other assertion.
    assert_eq!(osc::fx_type_short(10, 1), "DLY", "type 10 = Stereo Delay");
    assert_eq!(osc::fx_type_short(11, 1), "3TAP", "type 11 = 3-Tap Delay");
    assert_eq!(osc::fx_type_short(12, 1), "4TAP", "type 12 = 4-Tap Delay");
    assert_eq!(
        osc::fx_type_short(26, 1),
        "MODD",
        "type 26 = Modulated Delay"
    );
    assert_eq!(osc::fx_type_short(0, 1), "HALL", "type 0 = Hall Reverb");
}

#[test]
fn fx_type_long_bus_slot_all_type_ids() {
    for type_id in 0i32..=60 {
        let s = osc::fx_type_long(type_id, 1);
        assert!(
            !s.is_empty(),
            "type_id {type_id} long label must not be empty"
        );
    }
    assert_eq!(osc::fx_type_long(999, 1), "Unknown");
    // Spot-check BPM-compatible effects — wrong labels here would silently produce
    // confusing UI text without breaking any other assertion.
    assert_eq!(osc::fx_type_long(10, 1), "Stereo Delay");
    assert_eq!(osc::fx_type_long(11, 1), "3-Tap Delay");
    assert_eq!(osc::fx_type_long(12, 1), "4-Tap Delay");
    assert_eq!(osc::fx_type_long(26, 1), "Modulated Delay");
    assert_eq!(osc::fx_type_long(0, 1), "Hall Reverb");
}

#[test]
fn fx_type_short_insert_slot_all_type_ids() {
    // Insert slots 5–8 use the Sfxtyp2 table (34 known types + unknown fallback).
    for type_id in 0i32..=33 {
        let s = osc::fx_type_short(type_id, 5);
        assert!(
            !s.is_empty(),
            "insert type_id {type_id} short label must not be empty"
        );
        assert!(
            s.len() <= 4,
            "short label for insert type_id {type_id} should be ≤4 chars: {s:?}"
        );
    }
    assert_eq!(osc::fx_type_short(999, 5), "???");
    // Slot 8 (highest insert slot) must also use insert table.
    assert_ne!(osc::fx_type_short(0, 8), osc::fx_type_short(0, 1));
    // Spot-check known insert-slot types.
    assert_eq!(
        osc::fx_type_short(0, 5),
        "GEQ2",
        "insert type 0 = Dual GEQ 27"
    );
    assert_eq!(osc::fx_type_short(6, 5), "P1A", "insert type 6 = PCM42");
}

#[test]
fn fx_type_long_insert_slot_all_type_ids() {
    for type_id in 0i32..=33 {
        let s = osc::fx_type_long(type_id, 5);
        assert!(
            !s.is_empty(),
            "insert type_id {type_id} long label must not be empty"
        );
    }
    assert_eq!(osc::fx_type_long(999, 6), "Unknown");
    // Spot-check a few known insert-slot types to catch table mix-ups.
    assert_eq!(osc::fx_type_long(0, 5), "Dual GEQ 27");
    assert_eq!(osc::fx_type_long(6, 5), "PCM42");
    assert_eq!(osc::fx_type_long(23, 5), "Stereo Imager");
}

#[test]
fn is_bpm_compatible_bus_slot_bpm_types() {
    let bpm_types = [10i32, 11, 12, 21, 24, 25, 26];
    for slot in 1u8..=4 {
        for &t in &bpm_types {
            assert!(
                osc::is_bpm_compatible(t, slot),
                "type {t} slot {slot} should be BPM-compatible"
            );
        }
        // Non-delay types must not be compatible.
        assert!(
            !osc::is_bpm_compatible(0, slot),
            "HALL reverb must not be BPM-compatible"
        );
        assert!(
            !osc::is_bpm_compatible(33, slot),
            "P1A2 must not be BPM-compatible"
        );
    }
}

#[test]
fn is_bpm_compatible_insert_slots_always_false() {
    // Insert slots 5–8 contain no delay effects.
    for slot in 5u8..=8 {
        for type_id in [10i32, 11, 26] {
            assert!(
                !osc::is_bpm_compatible(type_id, slot),
                "insert slot {slot} type {type_id} must never be BPM-compatible"
            );
        }
    }
}

#[test]
fn delay_par_fallback_for_unknown_type() {
    // Any type_id not in the explicit match falls back to par/02.
    let bytes = osc::set_fx_delay(1, 99, 0.5);
    let msg = decode_msg(&bytes).expect("must decode");
    assert_eq!(
        msg.addr, "/fx/1/par/02",
        "unknown type should fall back to par/02"
    );
}
