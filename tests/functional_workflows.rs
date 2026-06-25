//! Functional workflow tests — end-to-end scenarios exercising the plugin as a
//! DAW host would: transport advancing through beat boundaries, BPM changes,
//! sync mode switching mid-session, and MIDI clock phase alignment.
//!
//! These sit above the unit/integration layer: they exercise multi-step
//! sequences that the existing param-automation matrix and network-worker tests
//! do not cover (Continuous mode, quantised hard reset, BPM change gap, etc.).
//!
//! All tests use the same infrastructure as `harness_e2e.rs` and
//! `sync_matrix.rs`: `vst_runtime::Harness` + `MockMixer` (UDP/OSC) +
//! `LoopbackClockSink` (in-process MIDI). Gated behind the `vst-runtime`
//! feature flag, serialised on `E2E_LOCK`.
#![cfg(not(feature = "standalone"))]

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::harness_util::{
    connect, drain_auto_sync, step, step_at, step_n_beats, step_until, wait_for_audit, E2E_LOCK,
};
use common::{MockMixer, SlotState};
use ethertap::EtherTap;
use mock_suite::loopback_sink::LoopbackClockSink;
use vst_runtime::Harness;

// ── MIDI helpers ─────────────────────────────────────────────────────────────

/// Register a loopback MIDI port and wire the harness's clock worker to it.
/// Panics if the bridge does not connect within 5 s.
fn setup_midi_loopback(harness: &mut Harness<EtherTap>, tag: &str) -> LoopbackClockSink {
    let name = format!(
        "EtherTap Functional {} {} {:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    );
    let sink = LoopbackClockSink::start_named(&name).expect("loopback sink register");
    let params = harness.plugin().ethertap_params();
    let handles = harness.plugin().test_handles();
    *params.midi_out_device.lock() = Some(name.clone());
    handles
        .device_change_tx
        .send(Some(name))
        .expect("device_change send");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handles.midi_bridge_connected.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "MIDI worker never connected to loopback sink"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    sink
}

// ── Test 1: Full lifecycle ────────────────────────────────────────────────────

/// connect → auto-sync → disconnect → reconnect → re-sync.
#[test]
fn full_lifecycle_connect_disconnect_reconnect() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Disconnect.
    assert!(harness.set_param_normalized("disconnect", 1.0));
    let disconnected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(0.0)
    });
    assert!(disconnected, "is_connected never dropped after disconnect");
    let count_after_disconnect = mock.sync_count(1);

    // Quiesce — no further syncs while disconnected.
    for _ in 0..10 {
        step(&mut harness, 120.0);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        mock.sync_count(1),
        count_after_disconnect,
        "sync arrived while disconnected"
    );

    // Reconnect via connect_to_last.
    assert!(harness.set_param_normalized("connect_to_last", 1.0));
    let reconnected = step_until(&mut harness, 120.0, Duration::from_secs(5), |h| {
        h.param_normalized("is_connected") == Some(1.0)
    });
    assert!(reconnected, "never reconnected after connect_to_last");

    // Auto-sync fires after reconnect.
    let synced_again = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(1) > count_after_disconnect
    });
    assert!(synced_again, "no sync after reconnect");

    harness.deactivate();
}

// ── Test 2: Rate OnChange — multiple BPM changes ──────────────────────────────

/// BPM change → settle (500 ms) → dispatch; repeat with a second BPM change to
/// verify the settle timer re-arms correctly.
#[test]
fn rate_on_change_multiple_bpm_changes_each_settle() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    // Default rate_sync_mode = OnChange; let the initial auto-sync drain.
    let _ = step_until(&mut harness, 120.0, Duration::from_secs(2), |_| {
        mock.sync_count(1) > 0
    });
    std::thread::sleep(Duration::from_millis(300));
    let base = mock.sync_count(1);

    // First BPM change: 120 → 100. Let settle fire.
    let dispatched_100 = step_until(&mut harness, 100.0, Duration::from_secs(4), |_| {
        mock.sync_count(1) > base
    });
    assert!(
        dispatched_100,
        "OnChange did not dispatch after first BPM change (120→100)"
    );
    let rx = mock.rx_bpm(1).expect("slot 1 BPM should be set");
    assert!(
        (rx - 100.0).abs() < 0.5,
        "dispatched BPM should be 100, got {rx}"
    );
    let after_100 = mock.sync_count(1);

    // Second BPM change: 100 → 80. Let settle fire again.
    let dispatched_80 = step_until(&mut harness, 80.0, Duration::from_secs(4), |_| {
        mock.sync_count(1) > after_100
    });
    assert!(
        dispatched_80,
        "OnChange did not dispatch after second BPM change (100→80)"
    );
    let rx2 = mock.rx_bpm(1).expect("slot 1 BPM after second change");
    assert!(
        (rx2 - 80.0).abs() < 0.5,
        "dispatched BPM should be 80, got {rx2}"
    );

    harness.deactivate();
}

// ── Test 3: Rate Continuous — beat-boundary sync ──────────────────────────────

/// Continuous rate mode dispatches one sync per beat crossing while transport
/// is playing.
#[test]
fn rate_continuous_fires_on_each_beat_boundary() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Switch to Continuous rate mode.
    harness.set_param_normalized("rate_sync_mode", 1.0); // Continuous
    step(&mut harness, 120.0); // let atom mirror propagate

    let before = mock.sync_count(1);

    // Advance 4 beat boundaries at 120 BPM. last_beat_idx is 0 after the
    // prior step() calls, so crossings at beats 1..4 each fire a sync.
    step_n_beats(&mut harness, 4, 120.0, 0);
    std::thread::sleep(Duration::from_millis(300)); // drain OSC channel

    let fired = mock.sync_count(1) - before;
    // 4 beats → 4 crossings → 4 syncs. Upper bound of 5 allows for one in-flight
    // OSC message or buffer-boundary alignment rounding. Bound of 6+ would let a
    // double-fire bug (sync fires twice per crossing) pass undetected.
    assert!(
        (4..=5).contains(&fired),
        "expected 4–5 continuous rate syncs across 4 beats, got {fired}"
    );

    harness.deactivate();
}

// ── Test 4: Phase OnChange — quantised bar boundary ───────────────────────────

/// phase_sync_mode=OnChange: BPM settle arms a quantised Hard Reset that fires
/// at the next bar boundary (not immediately). Verify mute → set → unmute
/// sequence arrives in the correct order.
#[test]
fn phase_on_change_quantised_hard_reset_at_bar_boundary() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Enable phase OnChange mode (rate stays OnChange by default).
    harness.set_param_normalized("phase_sync_mode", 0.5); // OnChange
    step(&mut harness, 120.0);
    mock.clear_msgs();

    // Change BPM 120 → 100: starts the settle timer. step() keeps pos_beats=0
    // so hr_target_beat will be 4.0 (start of bar 2) when settle fires.
    let _ = step_until(&mut harness, 100.0, Duration::from_secs(3), |_| false);

    // Advance transport through 5 beats to cross the bar 1 boundary (beat 4.0).
    // The quantised Hard Reset fires at hr_target_beat and sends
    // HardResetBatch → network worker → mute → delay-set → unmute.
    step_n_beats(&mut harness, 5, 100.0, 0);
    std::thread::sleep(Duration::from_millis(500)); // let OSC complete

    let msgs = mock.received_filtered(|m| {
        m.is_mute().map(|(s, _)| s == 1).unwrap_or(false)
            || m.is_set_delay().map(|(s, _)| s == 1).unwrap_or(false)
    });
    let mute_idx = msgs.iter().position(|m| m.is_mute() == Some((1, true)));
    let unmute_idx = msgs.iter().position(|m| m.is_mute() == Some((1, false)));
    match (mute_idx, unmute_idx) {
        (Some(m), Some(u)) => {
            assert!(m < u, "unmute arrived before mute (m={m}, u={u})");
            let set_between = msgs[m..u]
                .iter()
                .any(|msg| msg.is_set_delay().is_some_and(|(s, v)| s == 1 && v > 0.0));
            assert!(
                set_between,
                "no delay-time set between mute ({m}) and unmute ({u})"
            );
        }
        other => panic!("hard reset sequence incomplete: (mute_idx, unmute_idx) = {other:?}"),
    }

    harness.deactivate();
}

// ── Test 5: Phase Continuous — hard reset every beat ─────────────────────────

/// phase_sync_mode=Continuous: every beat crossing triggers a Hard Reset
/// (mute → delay-set → unmute). Verify ≥ 3 complete sequences over 4 beats.
#[test]
fn phase_continuous_hard_reset_on_each_beat() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);
    mock.clear_msgs();

    // Continuous phase mode: every beat boundary fires a Hard Reset.
    harness.set_param_normalized("phase_sync_mode", 1.0); // Continuous
    step(&mut harness, 120.0);

    // Advance through 4 beat boundaries.
    step_n_beats(&mut harness, 4, 120.0, 0);
    std::thread::sleep(Duration::from_millis(500)); // let all OSC sequences drain

    let mute_count = mock
        .received_filtered(|m| m.is_mute() == Some((1, true)))
        .len();
    let unmute_count = mock
        .received_filtered(|m| m.is_mute() == Some((1, false)))
        .len();
    // 4 beats → 4 Hard Resets → 4 mute/unmute pairs. Lower bound of 3 allows for an
    // off-by-one at the first beat boundary; upper bound of 5 allows for one extra
    // in-flight OSC sequence. Bound of 6+ would let a double-fire bug (two batches
    // per crossing) pass undetected.
    assert!(
        (3..=5).contains(&mute_count),
        "expected 3–5 hard reset mutes over 4 beats, got {mute_count}"
    );
    assert!(
        (3..=5).contains(&unmute_count),
        "expected 3–5 hard reset unmutes over 4 beats, got {unmute_count}"
    );
    assert_eq!(
        mute_count, unmute_count,
        "every mute must have a matching unmute (no slot left muted)"
    );

    harness.deactivate();
}

// ── Test 6: Sync mode switch mid-session ──────────────────────────────────────

/// Switch rate_sync_mode from Manual → Continuous mid-session. No syncs fire
/// while Manual; syncs fire on every beat after switching to Continuous.
#[test]
fn rate_mode_switch_manual_to_continuous_mid_session() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Manual rate mode: no auto-syncs while playing.
    harness.set_param_normalized("rate_sync_mode", 0.0); // Manual
    step(&mut harness, 120.0);
    let before_manual = mock.sync_count(1);

    step_n_beats(&mut harness, 3, 120.0, 0);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        mock.sync_count(1),
        before_manual,
        "Manual mode must not auto-sync on beat crossings"
    );

    // Beat 3 sample position — manual phase ended here (step_n_beats(3, 0) →
    // last buffer at 66150 = beat 3.0 exactly).
    let samples_3_beats = (3.0 * 44_100.0 * 60.0 / 120.0) as i64; // = 66150

    // Switch to Continuous. Use step_at at the *current* position (beat 3) to
    // propagate the atom without triggering the backward-seek path — step()
    // always goes to pos=0 which, after the transport was at beat 3, would
    // cause a backward seek: last_beat_idx resets to -1, hr_pending fires a
    // Hard Reset at hr_target=0 (pos=0 >= 0 → immediate), and section 6 fires
    // a Continuous sync (beat 0 > -1). Both arrive at the mock asynchronously,
    // inflating the `fired` count and masking a double-fire bug.
    harness.set_param_normalized("rate_sync_mode", 1.0); // Continuous
    step_at(&mut harness, 120.0, samples_3_beats); // mirror atom; last_beat_idx stays 3
    let before_continuous = mock.sync_count(1);

    // Advance 3 more beats from where we left off (beat 3).
    step_n_beats(&mut harness, 3, 120.0, samples_3_beats);
    std::thread::sleep(Duration::from_millis(300));

    let fired = mock.sync_count(1) - before_continuous;
    // last_beat_idx = 3 after the mode-switch step → crossings at beats 4, 5,
    // 6 = 3 syncs. Upper bound of 4 allows for one extra if a first-buffer
    // boundary coincidence fires at beat 3 as well. Bound of 5+ would let a
    // double-fire bug (two syncs per crossing) pass undetected.
    assert!(
        (3..=4).contains(&fired),
        "expected 3–4 syncs after switching to Continuous (3 beats), got {fired}"
    );

    harness.deactivate();
}

// ── Test 7: Tempo switching ───────────────────────────────────────────────────

/// Multiple BPM changes with settle verification: 120 → 140 → 90.
/// Each settle dispatches the correct BPM to the mixer.
#[test]
fn tempo_switching_three_changes_each_settle() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);

    // rate_sync_mode=OnChange (default). Wait for initial sync.
    let _ = step_until(&mut harness, 120.0, Duration::from_secs(2), |_| {
        mock.sync_count(1) > 0
    });
    std::thread::sleep(Duration::from_millis(300));
    let after_120 = mock.sync_count(1);

    // 120 → 140 BPM.
    let d140 = step_until(&mut harness, 140.0, Duration::from_secs(4), |_| {
        mock.sync_count(1) > after_120
    });
    assert!(d140, "settle not triggered for 140 BPM");
    let rx140 = mock.rx_bpm(1).expect("bpm after 140");
    assert!((rx140 - 140.0).abs() < 0.5, "expected 140 BPM, got {rx140}");
    let after_140 = mock.sync_count(1);

    // 140 → 90 BPM.
    let d90 = step_until(&mut harness, 90.0, Duration::from_secs(4), |_| {
        mock.sync_count(1) > after_140
    });
    assert!(d90, "settle not triggered for 90 BPM");
    let rx90 = mock.rx_bpm(1).expect("bpm after 90");
    assert!((rx90 - 90.0).abs() < 0.5, "expected 90 BPM, got {rx90}");

    harness.deactivate();
}

// ── Test 8: MIDI clock — transport start beat alignment ───────────────────────

/// When transport starts mid-beat, the clock worker waits for the next beat
/// boundary before forwarding 0xF8 bytes. Verify: zero ticks before beat 1.0,
/// then flow begins.
#[test]
fn midi_clock_transport_start_waits_for_beat_boundary() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap init");
    let sink = setup_midi_loopback(&mut harness, "transport-start");
    // midi_clock_enabled defaults true.

    // Transport was never played (was_playing=false). First step_at fires
    // TransportStart → waiting_for_beat=true. Start mid-beat at beat 0.5.
    let samples_per_beat = (44_100.0 * 60.0 / 120.0) as i64; // 22050 @ 120 BPM
    step_at(&mut harness, 120.0, samples_per_beat / 2); // pos_beats ≈ 0.5
    std::thread::sleep(Duration::from_millis(50));

    // No ticks should arrive: the only tick generated (k=12, off-beat) is
    // suppressed by waiting_for_beat.
    assert_eq!(
        sink.total_clocks(),
        0,
        "no ticks expected before first beat boundary after transport start"
    );

    // Advance continuously through beat 1.0 (pos_beats=1.0 → on_beat tick clears gate).
    // Drive 2 beats from beat 0.5+ε so tick 24 (on_beat) is reached in the stream.
    step_n_beats(&mut harness, 2, 120.0, samples_per_beat / 2 + 256);
    std::thread::sleep(Duration::from_millis(200));

    // After beat alignment: clock runs from beat 1.0 to ~2.5 = ≥1 full beat = ≥24 ticks.
    assert!(
        sink.total_clocks() >= 24,
        "expected at least 1 full beat of ticks after alignment, got {}",
        sink.total_clocks()
    );

    harness.deactivate();
}

// ── Test 9: MIDI clock — BPM change silence gap ───────────────────────────────

/// A BPM change > 0.5 BPM inserts a ≥ 1 500 ms silence gap. Verify ticks stop
/// during the gap and resume after it expires.
#[test]
fn midi_clock_bpm_change_inserts_silence_gap() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap init");
    let sink = setup_midi_loopback(&mut harness, "bpm-gap");

    // Establish clock running at 120 BPM (2 beats = 48 ticks at PPQ=24).
    let pos_after_base = step_n_beats(&mut harness, 2, 120.0, 0);
    std::thread::sleep(Duration::from_millis(200));
    let baseline = sink.total_clocks();
    assert!(
        baseline >= 2 * 24,
        "need baseline ticks before BPM change, got {baseline}"
    );

    // Change BPM 120 → 160 (delta=40 > 0.5 BPM_MIDI_THRESHOLD): BpmChanged fires.
    // MIN_RESYNC_GAP_MS = 1 500 ms, so the gap is always ≥ 1.5 s.
    step_at(&mut harness, 160.0, pos_after_base);

    // Advance 1 beat at 160 BPM (~325 ms real time) — still well within the gap.
    let pos_in_gap = step_n_beats(&mut harness, 1, 160.0, pos_after_base + 256);
    let mid_gap_count = sink.total_clocks();
    // Allow +2 for in-flight ticks already queued before BpmChanged propagates.
    assert!(
        mid_gap_count <= baseline + 2,
        "ticks must be suppressed during BPM change gap (baseline={baseline}, got {mid_gap_count})"
    );

    // Wait for the gap to expire (1 500 ms minimum).  We've already spent
    // ~325 ms in step_n_beats, so sleeping another 1 300 ms totals ~1 625 ms.
    std::thread::sleep(Duration::from_millis(1_300));

    // Drive 2 more beats at 160 BPM; clock should have resumed.
    let _pos_after_gap = step_n_beats(&mut harness, 2, 160.0, pos_in_gap);
    std::thread::sleep(Duration::from_millis(200));

    assert!(
        sink.total_clocks() > mid_gap_count + 10,
        "ticks must resume after BPM change gap (mid_gap={mid_gap_count}, after={})",
        sink.total_clocks()
    );

    harness.deactivate();
}

// ── Test 10: MIDI clock PPQ accuracy ─────────────────────────────────────────

/// Driving exactly N beats produces exactly N × PPQ clock bytes for each
/// supported PPQ value.
#[test]
fn midi_clock_ppq_accuracy() {
    // PPQ values: (normalized_param_value, ppq_u8)
    // EnumParam<Ppq> has 10 variants (P3=0..P96=9); norm = index/9.
    // P24 = index 6 → 6/9, P48 = index 8 → 8/9, P96 = index 9 → 1.0.
    let cases: &[(f32, u64)] = &[(6.0 / 9.0, 24), (8.0 / 9.0, 48), (1.0, 96)];

    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    for &(norm, ppq) in cases {
        let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap init");

        // Set PPQ before wiring the sink so the clock worker starts with
        // the correct rate from the first tick.
        harness.set_param_normalized("midi_clock_ppq", norm);
        step(&mut harness, 120.0); // mirror atom

        let tag = format!("ppq{ppq}");
        let sink = setup_midi_loopback(&mut harness, &tag);

        // Drive exactly 4 beats from position 0; the first step fires
        // TransportStart → waiting_for_beat=true.  Tick 0 (on_beat) in the
        // first buffer clears the gate immediately (pos_beats=0 ⟹ k=0 is
        // on_beat), so all 4 × PPQ ticks should arrive.
        step_n_beats(&mut harness, 4, 120.0, 0);
        std::thread::sleep(Duration::from_millis(300));

        let total = sink.total_clocks();
        let expected = 4 * ppq;
        // Buffer boundaries don't align to tick boundaries at all PPQ values,
        // so ceil() rounding accumulates up to ~1 extra tick per beat (4 beats →
        // up to 4 extra). Allow a small window above and below.
        let tolerance = (expected / 32).max(5);
        assert!(
            total >= expected - tolerance && total <= expected + tolerance,
            "PPQ {ppq}: expected ~{expected} ticks for 4 beats (±{tolerance}), got {total}"
        );

        assert_eq!(
            harness
                .plugin()
                .test_handles()
                .midi_clock_drop_count
                .load(Ordering::Relaxed),
            0,
            "PPQ {ppq}: no clock drops allowed"
        );

        harness.deactivate();
    }
}

// ── Test 11: Backward seek arms Hard Reset ───────────────────────────────────

/// A position rewind (pos_beats drops by > 0.5) arms `hr_pending` regardless
/// of sync mode and is reflected in the `phase_reset_pending` param.
#[test]
fn backward_seek_arms_phase_reset_pending() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut harness = Harness::<EtherTap>::new(44_100.0, 256).expect("EtherTap init");

    // Advance to beat 8 to establish last_pos_beats=8.0.
    let samples_per_beat = (44_100.0 * 60.0 / 120.0) as i64;
    let pos_beat8 = 8 * samples_per_beat;
    for _ in 0..5 {
        step_at(&mut harness, 120.0, pos_beat8);
    }

    // Rewind to beat 1.5 (8.0 - 1.5 = 6.5 > 0.5 threshold). hr_target_beat
    // = ceil(1.5) = 2.0, which is above current pos_beats, so the HR gate
    // (section 5) does NOT fire in the same call — hr_pending stays true.
    let pos_beat1_5 = (1.5 * 44_100.0 * 60.0 / 120.0) as i64;
    step_at(&mut harness, 120.0, pos_beat1_5);
    // Second step at the same position lets the param update propagate
    // (process() compares hr_pending to last_phase_reset_pending).
    step_at(&mut harness, 120.0, pos_beat1_5);

    assert_eq!(
        harness.param_normalized("phase_reset_pending"),
        Some(1.0),
        "backward seek must arm phase_reset_pending"
    );

    harness.deactivate();
}

// ── Test 12: FX slot switch mid-sync ─────────────────────────────────────────

/// In single-slot mode, switching fx_slot redirects subsequent force-sync
/// dispatches to the new slot without touching the old one.
#[test]
fn fx_slot_switch_mid_sync_targets_new_slot_only() {
    let _guard = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Both slots 1 and 2 are DLY so the audit marks them both compatible.
    let mut slots = [SlotState::empty(); 8];
    slots[0] = SlotState::dly(120.0);
    slots[1] = SlotState::dly(120.0);
    let mock = MockMixer::start_with_slots(slots);
    let mut harness = connect(&mock);
    wait_for_audit(&mut harness);
    drain_auto_sync(&mut harness, &mock);

    // Switch to single-slot mode targeting slot 1.
    harness.set_param_normalized("all_slots", 0.0);
    harness.set_param_normalized("fx_slot", 0.0); // slot 1 (norm=(1-1)/7=0)
    step(&mut harness, 120.0); // let atoms mirror

    let before = [mock.sync_count(1), mock.sync_count(2)];
    harness.set_param_normalized("force_sync_rate", 1.0);
    let s1_synced = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(1) > before[0]
    });
    assert!(s1_synced, "slot 1 never received sync in single-slot mode");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        mock.sync_count(2),
        before[1],
        "slot 2 must not receive sync when fx_slot=1"
    );

    // Switch to slot 2 (norm=(2-1)/7 = 1/7).
    harness.set_param_normalized("fx_slot", 1.0 / 7.0);
    step(&mut harness, 120.0); // mirror
    let mid = [mock.sync_count(1), mock.sync_count(2)];

    harness.set_param_normalized("force_sync_rate", 1.0);
    let s2_synced = step_until(&mut harness, 120.0, Duration::from_secs(5), |_| {
        mock.sync_count(2) > mid[1]
    });
    assert!(s2_synced, "slot 2 never received sync after slot switch");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        mock.sync_count(1),
        mid[0],
        "slot 1 must not receive sync after switching to fx_slot=2"
    );

    // Verify BPM is correct on both slots.
    let rx1 = mock.rx_bpm(1).expect("slot 1 BPM");
    let rx2 = mock.rx_bpm(2).expect("slot 2 BPM");
    assert!(
        (rx1 - 120.0).abs() < 0.5,
        "slot 1 BPM should be 120, got {rx1}"
    );
    assert!(
        (rx2 - 120.0).abs() < 0.5,
        "slot 2 BPM should be 120, got {rx2}"
    );

    harness.deactivate();
}
