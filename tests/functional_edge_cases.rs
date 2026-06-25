//! Edge-case workflow tests — scenarios that sit between normal operation and
//! error recovery, exercising timing interactions, mode combinations, extreme
//! values, and mid-session state transitions.
//!
//! All tests use the same harness + MockMixer + LoopbackClockSink
//! infrastructure as `functional_workflows.rs`. Gated on
//! `#![cfg(not(feature = "standalone"))]`.
#![cfg(not(feature = "standalone"))]

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::harness_util::{
    connect, drain_auto_sync, step, step_at, step_at_opts, step_at_stopped_with_pos, step_n_beats,
    step_no_pos, step_stopped, step_until, wait_for_audit, E2E_LOCK,
};
use common::{MockMixer, SlotState};
use ethertap::EtherTap;
use mock_suite::loopback_sink::LoopbackClockSink;
use vst_runtime::Harness;

// ── helper ───────────────────────────────────────────────────────────────────

fn midi_loopback(harness: &mut Harness<EtherTap>, tag: &str) -> LoopbackClockSink {
    let name = format!(
        "EtherTap Edge {} {} {:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    );
    let sink = LoopbackClockSink::start_named(&name).expect("loopback register");
    let params = harness.plugin().ethertap_params();
    let handles = harness.plugin().test_handles();
    *params.midi_out_device.lock() = Some(name.clone());
    handles.device_change_tx.send(Some(name)).expect("send");
    let dl = Instant::now() + Duration::from_secs(5);
    while !handles.midi_bridge_connected.load(Ordering::Acquire) {
        assert!(Instant::now() < dl, "MIDI bridge never connected");
        std::thread::sleep(Duration::from_millis(20));
    }
    sink
}

// ── Test 1: force sync fires immediately during BPM settle ───────────────────

/// If the user hits Force Sync while the BPM settle timer is running, the sync
/// dispatches immediately (at the current BPM) without waiting for the 500 ms
/// settle window to expire.
#[test]
fn force_sync_during_settle_dispatches_immediately() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // rate_sync_mode=OnChange (default). Change BPM to start settle timer.
    // Immediately force sync before 500 ms elapses.
    let before = mock.sync_count(1);
    harness.set_param_normalized("force_sync_rate", 1.0);
    let synced = step_until(&mut harness, 100.0, Duration::from_millis(400), |_| {
        mock.sync_count(1) > before
    });
    assert!(
        synced,
        "force sync should dispatch immediately, not wait for settle"
    );

    // The dispatched BPM should be the new 100 BPM, not the previous 120.
    let rx = mock.rx_bpm(1).expect("bpm after force sync");
    assert!((rx - 100.0).abs() < 0.5, "expected 100 BPM, got {rx}");

    harness.deactivate();
}

// ── Test 2: OnChange does not dispatch when transport is stopped ──────────────

/// A BPM change and settle while transport is stopped must not trigger a
/// dispatch. The dispatch gate requires `playing` to be true.
#[test]
fn rate_on_change_no_dispatch_when_stopped() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    let before = mock.sync_count(1);

    // Advance with transport stopped while BPM changes (100 BPM settle timer).
    // Wall-clock loop ensures we always exceed SETTLE_MS (500 ms) regardless
    // of OS sleep granularity.
    let settle_deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < settle_deadline {
        step_stopped(&mut harness, 100.0);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        mock.sync_count(1),
        before,
        "stopped transport: OnChange must not dispatch on BPM settle"
    );

    harness.deactivate();
}

// ── Test 3: PS3 — time-signature change arms Hard Reset ──────────────────────

/// Changing time signature from 3/4 → 4/4 (PS3) arms a quantised Hard Reset.
/// The HR fires at the next bar boundary and produces mute → set → unmute.
#[test]
fn time_sig_change_arms_hard_reset_ps3() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    let spb = (44_100.0 * 60.0 / 120.0) as i64; // samples per beat @ 120 BPM

    // step_at uses 4/4 → last_time_sig = (4, 4) after connect.
    // Step 1: switch to 3/4 — PS3 does NOT fire because last_time_sig == (4,4).
    step_at_opts(&mut harness, 120.0, spb, 3, 4, None);
    // Step 2: switch back to 4/4 — PS3 fires in section 6c (last=(3,4) ≠ (4,4)).
    // At beat 2.0 in 4/4, next_bar_beat = 4.0.  hr_pending becomes true here,
    // but phase_reset_pending is updated in section 3b which runs *before* 6c
    // in the same call, so it won't reflect yet.
    step_at_opts(&mut harness, 120.0, 2 * spb, 4, 4, None);
    // Step 3: extra step — section 3b now sees hr_pending=true and propagates
    // it to the phase_reset_pending param. Use step_at (4/4 same pos) so
    // section 5 doesn't fire (pos_beats=2.0 < hr_target=4.0).
    step_at(&mut harness, 120.0, 2 * spb);

    // phase_reset_pending should be true (HR armed, target beat not yet reached).
    assert_eq!(
        harness.param_normalized("phase_reset_pending"),
        Some(1.0),
        "PS3: phase_reset_pending should be armed after time-sig change"
    );

    // Advance through beat 4 (bar 1 boundary) → Hard Reset fires.
    step_n_beats(&mut harness, 3, 120.0, 2 * spb + 256);
    std::thread::sleep(Duration::from_millis(400));

    let msgs = mock.received_filtered(|m| {
        m.is_mute().map(|(s, _)| s == 1).unwrap_or(false)
            || m.is_set_delay().map(|(s, _)| s == 1).unwrap_or(false)
    });
    let mute = msgs.iter().position(|m| m.is_mute() == Some((1, true)));
    let unmute = msgs.iter().position(|m| m.is_mute() == Some((1, false)));
    assert!(
        mute.is_some() && unmute.is_some(),
        "PS3: hard reset sequence (mute/unmute) never arrived"
    );

    harness.deactivate();
}

// ── Test 4: loop-detect snaps Hard Reset to bar boundary ─────────────────────

/// When transport jumps backward to the loop start (PS4), the Hard Reset is
/// quantised to the next *bar* boundary, not the next beat, giving the mixer
/// more lead time to re-align before the music starts again.
#[test]
fn loop_detection_snaps_hr_to_bar_boundary() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    let loop_range = (0.0f64, 8.0f64); // 8-beat loop

    // Advance past beat 7.5 (= end-0.5) so the is_loop_repeat check passes.
    // The loop-repeat condition requires pre_seek_pos_beats >= end - 0.5 = 7.5.
    // 7.6 beats = 7.6 * spb samples.
    let start_near_end = (7.6 * 44_100.0 * 60.0 / 120.0) as i64;
    for i in 0..5 {
        step_at_opts(
            &mut harness,
            120.0,
            start_near_end + i * 256,
            4,
            4,
            Some(loop_range),
        );
    }
    std::thread::sleep(Duration::from_millis(30));
    // After 5 steps, last_pos_beats ≈ 7.646 (> 7.5 = end-0.5). ✓

    // Backward seek to beat 0 — loop repeat detected → hr_target = next bar (4.0).
    // A non-loop scrub (seek to beat 1.5, no loop_range) would snap to beat 2.0;
    // loop-detect snaps to the bar boundary so re-sync is musically tighter.
    step_at_opts(&mut harness, 120.0, 0, 4, 4, Some(loop_range));
    // Extra step: section 3b updates phase_reset_pending from the hr_pending set
    // in section 3c of the previous call (same ordering issue as PS3).
    step_at_opts(&mut harness, 120.0, 0, 4, 4, Some(loop_range));

    assert_eq!(
        harness.param_normalized("phase_reset_pending"),
        Some(1.0),
        "loop repeat: HR must be pending (deferred to bar boundary), not fired immediately"
    );

    // Advance to beat 4 (next bar boundary) → HR fires → mute at mock.
    step_n_beats(&mut harness, 5, 120.0, 256);
    std::thread::sleep(Duration::from_millis(400));

    let got_mute = !mock
        .received_filtered(|m| m.is_mute() == Some((1, true)))
        .is_empty();
    assert!(got_mute, "loop HR must fire at bar boundary (beat 4)");

    harness.deactivate();
}

// ── Test 5: all-slots Hard Reset covers every compatible slot ─────────────────

/// In all-slots mode, force_sync_phase sends HardResetBatch which mutes ALL
/// compatible slots, sets their delay times, then unmutes them.
#[test]
fn all_slots_hard_reset_covers_all_compatible() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Slots 1 and 3 are DLY (BPM-compatible in slots 1–4). Slot 2 is empty.
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0); // slot 1
    slots[2] = SlotState::dly(120.0); // slot 3
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    harness.set_param_normalized("force_sync_phase", 1.0);
    let done = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        !mock
            .received_filtered(|m| m.is_mute() == Some((1, false)))
            .is_empty()
            && !mock
                .received_filtered(|m| m.is_mute() == Some((3, false)))
                .is_empty()
    });
    assert!(done, "hard reset should unmute both slot 1 and slot 3");

    // Verify mute arrived for both slots.
    let mute1 = !mock
        .received_filtered(|m| m.is_mute() == Some((1, true)))
        .is_empty();
    let mute3 = !mock
        .received_filtered(|m| m.is_mute() == Some((3, true)))
        .is_empty();
    assert!(mute1, "slot 1 must be muted in all-slots hard reset");
    assert!(mute3, "slot 3 must be muted in all-slots hard reset");

    harness.deactivate();
}

// ── Test 6: BPM extreme values produce valid OSC floats ──────────────────────

/// At minimum BPM (20) and maximum (300), force sync must dispatch a valid
/// delay-time float that round-trips through osc::bpm_to_float / rx_bpm().
#[test]
fn bpm_extreme_values_valid_osc() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    for &bpm in &[20.0f64, 300.0f64] {
        let before = mock.sync_count(1);
        harness.set_param_normalized("force_sync_rate", 1.0);
        let ok = step_until(&mut harness, bpm, Duration::from_secs(5), |_| {
            mock.sync_count(1) > before
        });
        assert!(ok, "force sync at {bpm} BPM should reach mock");
        let rx = mock.rx_bpm(1).expect("bpm at extreme value");
        assert!(
            (rx - bpm).abs() < 1.0,
            "BPM {bpm}: expected ≈{bpm}, got {rx}"
        );
    }

    harness.deactivate();
}

// ── Test 7: PPQ change mid-session doubles tick rate ─────────────────────────

/// Changing midi_clock_ppq from 24 to 48 while the clock is running should
/// approximately double the tick rate from that point on.
#[test]
fn midi_ppq_switch_mid_session_changes_rate() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let sink = midi_loopback(&mut harness, "ppq-switch");

    // Baseline: 2 beats at PPQ=24 (default).
    let pos_after_base = step_n_beats(&mut harness, 2, 120.0, 0);
    std::thread::sleep(Duration::from_millis(100));
    let base = sink.total_clocks();
    // Expect ~48 ticks, allow tolerance.
    assert!(
        base >= 40,
        "need baseline ticks before PPQ switch, got {base}"
    );

    // Switch to PPQ=48 (index 8 of 10 variants → norm = 8/9).
    harness.set_param_normalized("midi_clock_ppq", 8.0 / 9.0);
    step(&mut harness, 120.0); // mirror atom

    // Drive 2 more beats at the new PPQ.
    let _pos2 = step_n_beats(&mut harness, 2, 120.0, pos_after_base);
    std::thread::sleep(Duration::from_millis(100));
    let new_ticks = sink.total_clocks() - base;

    // New rate ≈ 48×2 = 96; old rate was ≈ 24×2 = 48. Ratio ≈ 2×.
    // Allow ±20% tolerance.
    assert!(
        (76..=116).contains(&new_ticks),
        "after PPQ switch to 48, expected ~96 new ticks for 2 beats, got {new_ticks}"
    );

    harness.deactivate();
}

// ── Test 8: stopping transport quiesces the MIDI clock ───────────────────────

/// When transport transitions to stopped, the clock worker receives Stop and
/// suppresses pending ticks. No new 0xF8 bytes should arrive after stopping.
#[test]
fn midi_stop_quiesces_clock() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let sink = midi_loopback(&mut harness, "stop");

    // Run clock for 2 beats.
    let _pos = step_n_beats(&mut harness, 2, 120.0, 0);
    std::thread::sleep(Duration::from_millis(100));
    let before_stop = sink.total_clocks();
    assert!(
        before_stop >= 40,
        "need ticks before stop, got {before_stop}"
    );

    // Stop transport: a non-playing step triggers ClockMsg::Stop.
    step_stopped(&mut harness, 120.0);
    std::thread::sleep(Duration::from_millis(300)); // let worker drain

    let frozen = sink.total_clocks();
    // Drive more stopped frames — count must not grow.
    for _ in 0..30 {
        step_stopped(&mut harness, 120.0);
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        sink.total_clocks(),
        frozen,
        "clock must not emit ticks after transport stop (frozen={frozen})"
    );

    harness.deactivate();
}

// ── Test 9: connect while transport already playing ───────────────────────────

/// If the transport is already running when the user connects, the post-connect
/// auto-sync dispatches the *current* host BPM (not 0 or a stale value).
#[test]
fn connect_while_playing_dispatches_current_bpm() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(100.0);
    let mock = MockMixer::start_with_slots(slots);

    // Create harness but do NOT connect yet.
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    // Advance transport to beat 8 at 140 BPM — last_bpm will be set to 140.
    step_n_beats(&mut harness, 8, 140.0, 0);

    // Now connect.
    let params = harness.plugin().ethertap_params();
    *params.target_ip.lock() = "127.0.0.1".to_string();
    *params.target_port.lock() = mock.port();
    assert!(harness.set_param_normalized("connect_to_last", 1.0));

    let connected = step_until(&mut harness, 140.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(1.0)
    });
    assert!(connected, "connect while playing: never connected");

    // Wait for audit + auto-sync.
    let handles = harness.plugin().test_handles();
    let audited = step_until(&mut harness, 140.0, Duration::from_secs(5), |_| {
        handles
            .compatible_slots
            .load(std::sync::atomic::Ordering::Relaxed)
            != 0
    });
    assert!(audited, "slot audit never completed");

    let synced = step_until(&mut harness, 140.0, Duration::from_secs(5), |_| {
        mock.sync_count(1) > 0
    });
    assert!(synced, "auto-sync never fired after mid-flight connect");

    // Dispatch should carry 140 BPM, not 0 or 120.
    let rx = mock.rx_bpm(1).expect("bpm after mid-flight connect");
    assert!(
        (rx - 140.0).abs() < 1.0,
        "connect-while-playing: expected 140 BPM, got {rx}"
    );

    harness.deactivate();
}

// ── Test 10: backward seek fires Hard Reset at target beat ────────────────────

/// A backward seek arms hr_pending with hr_target_beat = ceil(seek_pos).
/// When transport advances to that beat, the Hard Reset dispatches
/// (mute → delay-set → unmute) to the mock.
#[test]
fn backward_seek_hard_reset_fires_at_target_beat() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    let spb = (44_100.0 * 60.0 / 120.0) as i64;

    // Advance to beat 8 to establish last_pos_beats = 8.
    for _ in 0..5 {
        step_at(&mut harness, 120.0, 8 * spb);
    }

    // Backward seek to beat 1.5 — hr_target_beat = ceil(1.5) = 2.0.
    // At pos_beats=1.5 section 5 does NOT fire (1.5 < 2.0), so HR stays pending.
    let pos_1_5 = (1.5 * 44_100.0 * 60.0 / 120.0) as i64;
    step_at(&mut harness, 120.0, pos_1_5);
    step_at(&mut harness, 120.0, pos_1_5); // param-update propagation step

    assert_eq!(
        harness.param_normalized("phase_reset_pending"),
        Some(1.0),
        "backward seek at beat 1.5 should arm phase_reset_pending"
    );

    // Advance through beat 2.0 → HR fires.
    step_n_beats(&mut harness, 2, 120.0, pos_1_5 + 256);
    std::thread::sleep(Duration::from_millis(400));

    let muted = !mock
        .received_filtered(|m| m.is_mute() == Some((1, true)))
        .is_empty();
    assert!(muted, "backward seek HR must fire (mute) at beat 2.0");

    harness.deactivate();
}

// ── Test 11: disconnect during settle prevents dispatch ───────────────────────

/// If the user disconnects while the BPM settle timer is running, the settle
/// fires but dispatch() bails (not connected). The mock must receive no new
/// syncs after the disconnect.
#[test]
fn disconnect_during_settle_prevents_dispatch() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Start BPM change → settle timer armed (500 ms).
    for _ in 0..5 {
        step(&mut harness, 100.0);
        std::thread::sleep(Duration::from_millis(10));
    }
    // 50 ms into settle — not done yet. Disconnect.
    harness.set_param_normalized("disconnect", 1.0);
    let disconnected = step_until(&mut harness, 100.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(0.0)
    });
    assert!(
        disconnected,
        "disconnect during settle: is_connected never dropped"
    );

    let count_after_disconnect = mock.sync_count(1);

    // Wait 700 ms more (> SETTLE_MS 500 ms) — settle would fire here if still
    // connected.  Wall-clock loop avoids false-pass when OS sleep resolves early.
    let settle_deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < settle_deadline {
        step(&mut harness, 100.0);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        mock.sync_count(1),
        count_after_disconnect,
        "settle must not dispatch after disconnect"
    );

    harness.deactivate();
}

// ── Test 12: Continuous rate + OnChange phase coexist ────────────────────────

/// rate_sync_mode=Continuous fires on every beat; phase_sync_mode=OnChange
/// only fires (Hard Reset) when BPM settles. The two must not interfere:
/// beats produce plain rate syncs, BPM change produces a subsequent HR.
#[test]
fn mixed_rate_continuous_phase_on_change_no_interference() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    harness.set_param_normalized("rate_sync_mode", 1.0); // Continuous
    harness.set_param_normalized("phase_sync_mode", 0.5); // OnChange
    step(&mut harness, 120.0); // mirror atoms

    let before = mock.sync_count(1);

    // 4 beats of Continuous rate syncs — must arrive without Hard Reset mutes.
    step_n_beats(&mut harness, 4, 120.0, 0);
    std::thread::sleep(Duration::from_millis(200));

    let rate_syncs = mock.sync_count(1) - before;
    assert!(
        rate_syncs >= 4,
        "expected ≥4 continuous rate syncs, got {rate_syncs}"
    );
    // No mutes during pure beat-crossing (phase=OnChange, not Continuous).
    let mutes_during_beats = mock.received_filtered(|m| m.is_mute().is_some()).len();
    assert_eq!(
        mutes_during_beats, 0,
        "no hard resets should fire on beats (phase=OnChange, not Continuous)"
    );

    // Now trigger a BPM change → settle → Hard Reset at next bar.
    let after_beats = mock.sync_count(1);
    let _ = step_until(&mut harness, 100.0, Duration::from_secs(3), |_| false);
    // hr_target_beat is 4 (pos_beats=0 throughout step_until; next_bar=4).
    step_n_beats(&mut harness, 5, 100.0, 0);
    std::thread::sleep(Duration::from_millis(500));

    // HR must arrive after BPM change.
    let mutes_after_change = mock.received_filtered(|m| m.is_mute().is_some()).len();
    assert!(
        mutes_after_change >= 2, // at least one mute + one unmute
        "hard reset must fire after OnChange settle (got {} mutes/unmutes)",
        mutes_after_change
    );
    // Additional rate syncs also keep arriving (Continuous still active).
    assert!(
        mock.sync_count(1) > after_beats,
        "Continuous rate syncs should continue after phase HR"
    );

    harness.deactivate();
}

// ── Test 13: rapid connect/disconnect cycle stabilises ───────────────────────

/// Three rapid connect/disconnect cycles must leave the plugin in a stable,
/// connected state — no deadlock, no panic, no stuck atomics.
#[test]
fn rapid_connect_disconnect_stable_end_state() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockMixer::start();
    let mut harness = connect(&mock);

    for cycle in 0..3 {
        harness.set_param_normalized("disconnect", 1.0);
        let disconnected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
            h.param_normalized("is_connected") == Some(0.0)
        });
        assert!(disconnected, "cycle {cycle}: never disconnected");

        harness.set_param_normalized("connect_to_last", 1.0);
        let reconnected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
            h.param_normalized("is_connected") == Some(1.0)
        });
        assert!(reconnected, "cycle {cycle}: never reconnected");
    }

    // After 3 cycles: stable connection, heartbeats flowing.
    assert!(
        mock.wait_for_addr("/info", Duration::from_secs(5)),
        "no /info heartbeat after rapid cycles"
    );

    harness.deactivate();
}

// ── Test 14: Continuous mode proportional over 16 beats ──────────────────────

/// Over a long run (16 beats) in Continuous mode, the sync count is
/// proportional to the number of beat crossings within ±2.
#[test]
fn continuous_rate_long_run_proportional() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    harness.set_param_normalized("rate_sync_mode", 1.0); // Continuous
    step(&mut harness, 120.0); // mirror
    let before = mock.sync_count(1);

    step_n_beats(&mut harness, 16, 120.0, 0);
    std::thread::sleep(Duration::from_millis(300));

    let fired = mock.sync_count(1) - before;
    assert!(
        (14..=18).contains(&fired),
        "16 beats Continuous: expected 14–18 syncs, got {fired}"
    );

    harness.deactivate();
}

// ── Test 15: OnChange settle in single-slot mode targets one slot only ────────

/// When rate_sync_mode=OnChange and all_slots=false, the settle dispatch goes
/// to the selected fx_slot only — not to other compatible slots.
#[test]
fn on_change_rate_single_slot_mode_targets_selected() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0); // slot 1
    slots[1] = SlotState::dly(120.0); // slot 2
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Single-slot mode targeting slot 2.
    harness.set_param_normalized("all_slots", 0.0);
    harness.set_param_normalized("fx_slot", 1.0 / 7.0); // slot 2 = (2-1)/7
    step(&mut harness, 120.0); // mirror atoms

    let before = [mock.sync_count(1), mock.sync_count(2)];

    // Trigger BPM settle (OnChange is default).
    let settled = step_until(&mut harness, 100.0, Duration::from_secs(4), |_| {
        mock.sync_count(2) > before[1]
    });
    assert!(
        settled,
        "OnChange settle in single-slot mode: slot 2 never received sync"
    );
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        mock.sync_count(1),
        before[0],
        "slot 1 must not receive a sync when fx_slot=2"
    );
    let rx = mock.rx_bpm(2).expect("slot 2 BPM after settle");
    assert!(
        (rx - 100.0).abs() < 0.5,
        "slot 2 should have settled to 100 BPM, got {rx}"
    );

    harness.deactivate();
}

// ── Test 16: MIDI clock tick accumulator without DAW transport position ────────

/// When a VST3 host does not supply transport position (pos_beats = None),
/// the MIDI clock worker falls back to a sample-count tick accumulator.
/// Verify ticks still reach the loopback sink and that the count is within
/// tolerance of PPQ × beats driven.
#[test]
fn midi_clock_runs_without_daw_transport_position() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let sink = midi_loopback(&mut harness, "no-pos");

    // Drive 4 beats without a DAW position — tick accumulator path.
    // 4 beats × 256-sample buffers @ 120 BPM: samples_per_beat = 22050,
    // so 4 beats = 88200 samples = 345 buffers.  Drive a few extra.
    for _ in 0..400 {
        step_no_pos(&mut harness, 120.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(150));

    // Expect approximately 4 × 24 = 96 ticks (±20 for buffer rounding).
    let total = sink.total_clocks();
    assert!(
        total >= 72,
        "tick accumulator: expected ≥72 ticks for ~4 beats, got {total}"
    );

    harness.deactivate();
}

// ── Test 17: BPM change rescales the tick accumulator phase ──────────────────

/// When the tick accumulator is running (no pos_beats) and BPM changes by
/// more than BPM_SETTLE_THRESHOLD, the accumulator is rescaled to preserve
/// phase continuity rather than resetting to zero. Verify: clock continues
/// after the BPM change with an approximately doubled tick rate.
#[test]
fn midi_clock_accumulator_bpm_change_rescales_phase() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let sink = midi_loopback(&mut harness, "accumulator-bpm");

    // Baseline: 2 beats at PPQ=24 (default) and 120 BPM.
    for _ in 0..180 {
        step_no_pos(&mut harness, 120.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(100));
    let base = sink.total_clocks();
    assert!(
        base >= 40,
        "need baseline ticks before BPM change, got {base}"
    );

    // Change BPM to 240 (delta=120 > BPM_SETTLE_THRESHOLD=0.01): rescale fires.
    // last_clock_bpm was 120 → new tick_interval halves → ~2× tick rate.
    for _ in 0..180 {
        step_no_pos(&mut harness, 240.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(100));
    let new_ticks = sink.total_clocks() - base;

    // At 240 BPM, PPQ=24: ~2 beats of ticks = ~48.  At 120 BPM baseline
    // we drove ~2 beats = ~48 ticks.  Ratio should be ≈ 2× (1.5×–3.5×).
    // Upper bound catches a pathological burst where ticks fire at an
    // unrestricted rate after the rescale (no floor on tick_interval).
    assert!(
        new_ticks as f64 >= base as f64 * 1.5,
        "after BPM 120→240 (rescale), expected ≥1.5× tick rate (base={base}, new={new_ticks})"
    );
    assert!(
        (new_ticks as f64) <= base as f64 * 3.5,
        "after BPM 120→240 (rescale), tick rate must not exceed 3.5× baseline (base={base}, new={new_ticks})"
    );

    harness.deactivate();
}

// ── Test 18: BPM settle timer restarts on significant mid-window change ────────

/// A BPM change large enough to exceed BPM_SETTLE_THRESHOLD (0.01) restarts
/// the settle timer (lib.rs line 720: first branch — not the re-arm guard at
/// expiry, which is what test 22 exercises with slow automation).  Verify:
/// no dispatch during the settle window; dispatch fires with the latest BPM
/// once the restarted timer expires.
///
/// Mechanism: phase 1 arms the timer at 100.0; the first phase-2 step has
/// |100.05 - 100.0| = 0.05 > 0.01, so the first branch fires and restarts
/// the timer with bpm_at_settle_start = 100.05.  The restarted timer expires
/// 500 ms into phase 2 (i.e. after phase 2 ends), so no dispatch occurs during
/// phases 1+2.  Phase 3 holds stable → timer fires → dispatch at 100.05.
#[test]
fn bpm_settle_restarts_on_large_change_during_window() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    let before = mock.sync_count(1);

    // Phase 1 (~300 ms): arms the settle timer with bpm_at_settle_start=100.0.
    // Wall-clock loop ensures at least 300 ms elapse.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(300) {
        step(&mut harness, 100.0);
        std::thread::sleep(Duration::from_millis(8));
    }

    // Phase 2 (~350 ms): delta = |100.05 - 100.0| = 0.05 > BPM_SETTLE_THRESHOLD.
    // The first step restarts the timer (bpm_at_settle_start = 100.05,
    // bpm_change_ts = now).  New expiry = ~300 ms + 500 ms = ~800 ms from test
    // start, which is after phase 2 ends (~650 ms).
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_millis(350) {
        step(&mut harness, 100.05);
        std::thread::sleep(Duration::from_millis(8));
    }

    // Timer was restarted and has not expired yet — no dispatch allowed here.
    assert_eq!(
        mock.sync_count(1),
        before,
        "settle must not dispatch during the restarted timer window"
    );

    // Phase 3: hold BPM stable at 100.05 — timer expires and dispatches.
    let dispatched = step_until(&mut harness, 100.05, Duration::from_secs(3), |_| {
        mock.sync_count(1) > before
    });
    assert!(
        dispatched,
        "dispatch must fire after the restarted settle timer expires"
    );

    // Dispatched value must reflect the BPM at restart time (100.05), not 100.0.
    let rx = mock.rx_bpm(1).expect("BPM after restarted settle dispatch");
    assert!(
        (rx - 100.05).abs() < 0.5,
        "dispatched BPM should be ~100.05 (bpm_at_settle_start at restart), got {rx}"
    );

    harness.deactivate();
}

// ── Test 19: MIDI bridge disconnects when device selection is cleared ─────────

/// When the user clears the MIDI output device selection (sends None), the
/// worker closes the current connection and sets bridge_connected = false.
#[test]
fn midi_bridge_cleared_device_disconnects() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let _sink = midi_loopback(&mut harness, "clear-device");
    let handles = harness.plugin().test_handles();

    // Loopback setup already verified bridge_connected = true.
    assert!(
        handles.midi_bridge_connected.load(Ordering::Acquire),
        "bridge must be connected after loopback setup"
    );

    // Clear the device selection.
    handles
        .device_change_tx
        .send(None)
        .expect("device_change send");

    // Wait for the worker to process the disconnect.
    let deadline = Instant::now() + Duration::from_secs(2);
    while handles.midi_bridge_connected.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "bridge never disconnected after device cleared"
        );
        step(&mut harness, 120.0);
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        !handles.midi_bridge_connected.load(Ordering::Acquire),
        "bridge_connected must be false after device selection cleared"
    );

    harness.deactivate();
}

// ── Test 20: MIDI bridge gracefully handles a device that is not present ──────

/// When the user selects a MIDI output device that does not exist (neither in
/// the loopback registry nor as a hardware port), try_connect_out returns None.
/// The bridge stays disconnected without panicking.
#[test]
fn midi_bridge_nonexistent_device_stays_disconnected() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let handles = harness.plugin().test_handles();

    // Request a device name that is guaranteed not to exist.
    handles
        .device_change_tx
        .send(Some("EtherTap Nonexistent Port ZZZ_99_test".to_string()))
        .expect("device_change send");

    // Give the worker time to attempt (and fail) the connect.
    std::thread::sleep(Duration::from_millis(200));
    step(&mut harness, 120.0);
    std::thread::sleep(Duration::from_millis(100));

    assert!(
        !handles.midi_bridge_connected.load(Ordering::Acquire),
        "bridge must not be connected for a nonexistent device"
    );

    harness.deactivate();
}

// ── Test 21: DAW transport stop sends ClockMsg::Stop ─────────────────────────

/// When the host transport stops while a DAW song position is set, the MIDI
/// clock worker receives ClockMsg::Stop and quiesces ticks.
/// Exercises lib.rs lines 867-878 and midi_clock.rs lines 465-473.
#[test]
fn midi_clock_daw_transport_stop_quiesces() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("init");
    let sink = midi_loopback(&mut harness, "daw-stop");

    // Advance 2 beats with DAW position to establish playing state.
    let pos_after = step_n_beats(&mut harness, 2, 120.0, 0);
    std::thread::sleep(Duration::from_millis(100));
    let before_stop = sink.total_clocks();
    assert!(
        before_stop >= 40,
        "need ticks before stop, got {before_stop}"
    );

    // Stop transport WITH a DAW position set — enters the DAW-mode if-branch
    // (`if let Some(beat_start) = pos_beats_raw`) in lib.rs section 7.
    // `was_playing = true` at this point, so ClockMsg::Stop is sent.
    step_at_stopped_with_pos(&mut harness, 120.0, pos_after);
    std::thread::sleep(Duration::from_millis(200));

    let frozen = sink.total_clocks();
    // Drive more stopped frames (with position); count must not grow.
    for i in 1..=20 {
        step_at_stopped_with_pos(&mut harness, 120.0, pos_after + (i as i64) * 256);
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        sink.total_clocks(),
        frozen,
        "MIDI clock must not emit ticks after DAW transport stop (frozen={frozen})"
    );

    harness.deactivate();
}

// ── Test 22: BPM slow automation triggers settle re-arm ───────────────────────

/// When BPM drifts slowly (< 0.01 BPM per step) over the 500 ms settle
/// window, the settle guard re-arms rather than dispatching at the wrong tempo.
/// Exercises lib.rs lines 734-737 (the re-arm branch inside the settle check).
#[test]
fn bpm_slow_automation_triggers_settle_rearm() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    let before = mock.sync_count(1);

    // Big BPM jump arms the settle timer: 120 → 100 (|20| > 0.01).
    // bpm_at_settle_start is set to 100.0.
    step(&mut harness, 100.0);

    // Drift BPM upward by 0.008 BPM per step.  Each inter-step delta = 0.008
    // < BPM_SETTLE_THRESHOLD (0.01), so line 720 never restarts the timer.
    // Wall-clock loop ensures we always exceed SETTLE_MS (500 ms): at expiry
    // bpm ≈ 100 + N×0.008 where N steps completed; |drift - 100.0| > 0.01 → re-arm.
    let mut drift_bpm = 100.0f64;
    let drift_deadline = Instant::now() + Duration::from_millis(700);
    while Instant::now() < drift_deadline {
        drift_bpm += 0.008;
        step(&mut harness, drift_bpm);
        std::thread::sleep(Duration::from_millis(10));
    }

    // Re-arm proof: settle fired during the drift window but must NOT have
    // dispatched (BPM changed since settle start).  Without re-arm, the first
    // settle expiry would have dispatched 100.0 and sync_count would exceed `before`.
    assert_eq!(
        mock.sync_count(1),
        before,
        "settle must not dispatch on first expiry when BPM is still drifting (re-arm path)"
    );

    // After re-arm, hold BPM stable so the second settle fires and dispatches.
    let dispatched = step_until(&mut harness, drift_bpm, Duration::from_secs(3), |_| {
        mock.sync_count(1) > before
    });
    assert!(
        dispatched,
        "dispatch must fire after BPM settle re-arm (slow automation path)"
    );

    harness.deactivate();
}

// ── Test 23: force_sync_phase in single-slot mode ─────────────────────────────

/// In single-slot mode, force_sync_phase (Hard Reset) must target only the
/// selected fx_slot — not every compatible slot.  All prior Hard Reset tests
/// either use all-slots mode (the post-connect default) or test at the
/// NetworkWorker level.  This test exercises the full plugin path:
/// force_sync_phase pulse → HardResetBatch → mock → verify correct slot targeted.
#[test]
fn force_sync_phase_single_slot_targets_selected_slot() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Both slots 1 and 3 are DLY (BPM-compatible) — audit marks both compatible.
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0); // slot 1
    slots[2] = SlotState::dly(120.0); // slot 3
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    // Single-slot mode targeting slot 3.
    harness.set_param_normalized("all_slots", 0.0);
    harness.set_param_normalized("fx_slot", 2.0 / 7.0); // slot 3 = (3-1)/7
    step(&mut harness, 120.0); // let atoms mirror

    // Trigger Hard Reset.
    harness.set_param_normalized("force_sync_phase", 1.0);
    let done = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        !mock
            .received_filtered(|m| m.is_mute() == Some((3, false)))
            .is_empty()
    });
    assert!(
        done,
        "slot 3 never received Hard Reset unmute in single-slot mode"
    );

    // Slot 3 must have received the full mute → set → unmute sequence.
    let muted3 = !mock
        .received_filtered(|m| m.is_mute() == Some((3, true)))
        .is_empty();
    assert!(muted3, "slot 3 must be muted during Hard Reset");

    // Slot 1 must be completely untouched — a single-slot Hard Reset must not
    // fall through to all-slots behavior.
    let muted1 = !mock
        .received_filtered(|m| m.is_mute().map(|(s, _)| s == 1).unwrap_or(false))
        .is_empty();
    assert!(
        !muted1,
        "slot 1 must not receive any mute in single-slot Hard Reset (fx_slot=3)"
    );

    harness.deactivate();
}
