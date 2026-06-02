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
fn roundtrip_fx_delay_par01_effects() {
    // 3TAP (11), 4TAP (12), MODD (26) use par/01
    let par01_effects = [(11, "3TAP"), (12, "4TAP"), (26, "MODD")];
    for (type_id, name) in par01_effects {
        let bytes = osc::set_fx_delay(2, type_id, 0.3);
        let msg = decode_msg(&bytes).expect("must decode");
        assert_eq!(msg.addr, "/fx/2/par/01", "{name} should use par/01");
        assert_eq!(msg.args, vec![OscType::Float(0.3)]);
    }
}

#[test]
fn roundtrip_fx_delay_combined_effects() {
    // D/RV (21), D/CR (24), D/FL (25) use par/01
    let par01_effects = [(21, "D/RV"), (24, "D/CR"), (25, "D/FL")];
    for (type_id, name) in par01_effects {
        let bytes = osc::set_fx_delay(3, type_id, 0.7);
        let msg = decode_msg(&bytes).expect("must decode");
        assert_eq!(msg.addr, "/fx/3/par/01", "{name} should use par/01");
        assert_eq!(msg.args, vec![OscType::Float(0.7)]);
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

// ─── Edge case: Very small/large delay_float values ────────────────────────────

#[test]
fn encode_edge_case_delay_float_zero() {
    let bytes = osc::set_fx_delay(1, 10, 0.0);
    let msg = decode_msg(&bytes).expect("must decode");
    assert_eq!(msg.args, vec![OscType::Float(0.0)]);
}

#[test]
fn encode_edge_case_delay_float_one() {
    let bytes = osc::set_fx_delay(1, 10, 1.0);
    let msg = decode_msg(&bytes).expect("must decode");
    assert_eq!(msg.args, vec![OscType::Float(1.0)]);
}

#[test]
fn encode_edge_case_delay_float_very_small() {
    // f32::MIN_POSITIVE is the smallest normal positive float
    // Test that it roundtrips without panicking and preserves value
    let bytes = osc::set_fx_delay(1, 10, f32::MIN_POSITIVE);
    let msg = decode_msg(&bytes).expect("must decode");
    let [OscType::Float(f)] = msg.args.as_slice() else { panic!("expected Float") };
    assert_eq!(*f, f32::MIN_POSITIVE, "MIN_POSITIVE should roundtrip exactly");
}

#[test]
fn encode_edge_case_delay_float_very_large() {
    // Near-max f32 should still encode without panicking
    let bytes = osc::set_fx_delay(1, 10, f32::MAX);
    let msg = decode_msg(&bytes).expect("must decode");
    let [OscType::Float(f)] = msg.args.as_slice() else { panic!("expected Float") };
    assert!(*f == f32::MAX);
}

// ─── Edge case: Empty OSC message ──────────────────────────────────────────────

#[test]
fn decode_empty_packet() {
    // rosc will return an error for an empty buffer; this is not a panic
    let result = rosc::decoder::decode_udp(&[]);
    assert!(result.is_err(), "empty buffer must return error");
}

#[test]
fn decode_totally_wrong_address() {
    // Valid OSC packet format but wrong address – should not panic
    use rosc::{encoder, OscMessage, OscPacket};
    let packet = OscPacket::Message(OscMessage {
        addr: "/this/does/not/exist".to_string(),
        args: vec![OscType::Int(42)],
    });
    let bytes = encoder::encode(&packet).unwrap();
    let result = rosc::decoder::decode_udp(&bytes);
    assert!(result.is_ok(), "should decode even if address is unknown");
}

// ─── Edge case: Wrong type tags ────────────────────────────────────────────────

#[test]
fn decode_wrong_type_tag_float_received_as_int() {
    // Craft a packet that looks like it has a float but has wrong type tag
    // rosc is lenient - just verify no panic
    let result = rosc::decoder::decode_udp(&[0x2f, 0x66, 0x78, 0x00, // "/fx\0"
                                             0x2f, 0x31, 0x00, 0x00, // "/1\0\0"
                                             0x2c, 0x69, 0x00, 0x00, // ",i\0\0"  (int type tag)
                                             0x00, 0x00, 0x00, 0x2a]); // int 42
    // Just verify no panic - rosc may succeed or fail
    let _ = result;
}

#[test]
fn decode_wrong_type_tag_int_received_as_float() {
    // Encode int but we don't know what rosc will interpret it as
    let packet = rosc::OscPacket::Message(rosc::OscMessage {
        addr: "/fx/1/par/02".to_string(),
        args: vec![OscType::Int(99)],
    });
    let bytes = rosc::encoder::encode(&packet).unwrap();
    let decoded = rosc::decoder::decode_udp(&bytes).unwrap().1;
    if let rosc::OscPacket::Message(m) = decoded {
        // Roundtrip as int since we encoded an int
        assert_eq!(m.args, vec![rosc::OscType::Int(99)]);
    }
}

// ─── Malformed packet handling – ensure no panics ──────────────────────────────

#[test]
fn malformed_packet_truncated_header() {
    // Truncated before OSC address complete – must not panic
    let result = rosc::decoder::decode_udp(b"/f");
    assert!(result.is_err());
}

#[test]
fn malformed_packet_truncated_type_tag() {
    // rosc is lenient; we just ensure no panic
    let result = rosc::decoder::decode_udp(&[
        0x2f, 0x66, 0x78, 0x00, // "/fx\0"
        0x00, 0x00, 0x00, 0x00, // padding
        0x2c, 0x66, 0x00,        // ",f\0" incomplete
    ]);
    // rosc may succeed or fail, but must not panic
    let _ = result;
}

#[test]
fn malformed_packet_garbage_after_valid_header() {
    // Valid OSC header followed by garbage – rosc is lenient, just verify no panic
    let result = rosc::decoder::decode_udp(&[
        0x2f, 0x69, 0x6e, 0x66, 0x6f, 0x00, 0x00, 0x00, // "/info\0\0\0"
        0x00, 0x00, 0x00, 0x00, // ","
        0xff, 0xff, 0xff, 0xff, // garbage
    ]);
    let _ = result;
}

#[test]
fn malformed_packet_zero_length_after_address() {
    // Valid address but nothing else – must not panic
    let result = rosc::decoder::decode_udp(&[
        0x2f, 0x69, 0x6e, 0x66, 0x6f, 0x00, 0x00, 0x00, // "/info\0\0\0"
    ]);
    let _ = result; // rosc is lenient; just ensure no panic
}

#[test]
fn malformed_packet_invalid_string_padding() {
    // Address string not null-terminated properly – must not panic
    let result = rosc::decoder::decode_udp(&[
        0x2f, 0x69, 0x6e, 0x66, 0x6f, 0x6f, 0x00, 0x00, // "/infoo\0\0" invalid
    ]);
    let _ = result; // rosc may reject but must not panic
}

// ─── Additional roundtrip consistency tests ────────────────────────────────────

#[test]
fn roundtrip_set_fxrtn_mute() {
    for slot in 1..=8 {
        for muted in [false, true] {
            let bytes = osc::set_fxrtn_mute(slot, muted);
            let msg = decode_msg(&bytes).expect("must decode");
            assert_eq!(msg.addr, format!("/fxrtn/{slot}/mix/on"));
            let expected = if muted { 0 } else { 1 };
            assert_eq!(msg.args, vec![OscType::Int(expected)]);
        }
    }
}

#[test]
fn roundtrip_query_fx_delay_all_effects() {
    for slot in 1..=4 {
        for type_id in [10, 11, 12, 21, 24, 25, 26] {
            let bytes = osc::query_fx_delay(slot, type_id);
            let msg = decode_msg(&bytes).expect("must decode");
            let expected_par = osc::delay_par(type_id);
            assert_eq!(
                msg.addr,
                format!("/fx/{slot}/par/{expected_par:02}"),
                "type {type_id} should use par/{expected_par}"
            );
            assert!(msg.args.is_empty(), "query must have no args");
        }
    }
}

#[test]
fn float_to_bpm_edge_cases() {
    // Zero (no data) returns sentinel 0.0
    assert_eq!(float_to_bpm(0.0), 0.0);
    
    // Very small positive values – should give very large BPM
    let small = 0.0001_f32;
    let bpm = float_to_bpm(small);
    assert!(bpm > 200_000.0, "small float should give large BPM, got {bpm}");
    
    // f32::MAX produces a tiny but still positive BPM (not zero)
    let bpm_max = float_to_bpm(f32::MAX);
    assert!(bpm_max > 0.0 && bpm_max < 1.0e-37, "f32::MAX should produce tiny positive BPM");
    
    // Sub-normal
    let subnormal = f32::MIN_POSITIVE / 2.0;
    let bpm = float_to_bpm(subnormal);
    assert!(bpm > 0.0, "sub-normal should produce valid BPM, got {bpm}");
}

#[test]
fn bpm_to_float_edge_cases() {
    // Very high BPM should clamp to ~0
    let f = bpm_to_float(10_000.0);
    assert!((0.0..=0.01).contains(&f), "extremely high BPM should be near-zero");
    
    // Very low BPM should clamp to 1.0
    let f = bpm_to_float(1.0);
    assert_eq!(f, 1.0, "extremely low BPM should be 1.0");
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
            error < 0.5,
            "BPM roundtrip error too high: {bpm} -> {f} -> {recovered}, error={error}"
        );
    }
}
