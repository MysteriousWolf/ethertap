//! Headless in-process driver for Rust-native [`nih_plug::Plugin`] implementations.
//!
//! `Harness<P>` mirrors the construction → process → teardown sequence that
//! `nih_export_standalone<P: Plugin>()` runs
//! (`vendor/nih-plug/src/wrapper/standalone/wrapper.rs:178-300` for construction,
//! `:521-536` for the process loop), but drives the plugin directly through its
//! trait methods instead of spinning up an audio backend or a window. This lets
//! a test script sequence `process()` calls over caller-supplied buffers and
//! observe the plugin's resulting state — zero IPC, zero serialization, the
//! plugin linked as a crate.
//!
//! Headless by construction: this crate never references `Plugin::editor()` or
//! `nih_plug_iced`.

use nih_plug::prelude::{
    AudioIOLayout, AuxiliaryBuffers, Buffer, BufferConfig, InitContext, Plugin, PluginApi,
    PluginNoteEvent, ProcessContext, ProcessMode,
};
/// Re-exported so integration tests can import `ProcessStatus` and `Transport`
/// from `vst_runtime` without adding a separate `nih_plug` dev-dependency.
pub use nih_plug::prelude::ProcessStatus;
pub use nih_plug::prelude::Transport;

/// Drives a single `P: Plugin` instance through its lifecycle headlessly.
///
/// Construction mirrors `Wrapper::new()`
/// (`vendor/nih-plug/src/wrapper/standalone/wrapper.rs:184-300`): build
/// `P::default()`, pick the plugin's default [`AudioIOLayout`], call
/// `initialize()`, then `reset()`. [`Harness::process`] then drives the
/// plugin's `process()` directly — the same call nih-plug's standalone backend
/// makes per period (`wrapper.rs:521-536`) — over caller-supplied buffers.
///
/// The harness's own driving loop is test/dev tooling, not RT-constrained: it
/// may allocate and block freely. Only the plugin's `process()` call needs to
/// see a realistic calling convention.
pub struct Harness<P: Plugin> {
    plugin: P,
    audio_io_layout: AudioIOLayout,
    sample_rate: f32,
}

/// A minimal [`InitContext`] for headless driving. No background-task queue, no
/// GUI — the plugin's `task_executor` is never invoked because the harness
/// never schedules tasks onto it.
struct HarnessInitContext;

impl<P: Plugin> InitContext<P> for HarnessInitContext {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Standalone
    }

    fn execute(&self, _task: P::BackgroundTask) {
        // No background executor in the harness. If a plugin schedules a task
        // here, it is silently dropped — which will cause silent malfunction if
        // the task is load-bearing (e.g. spawning a network thread in initialize).
        // Surface this loudly in debug builds so test authors can catch it early.
        debug_assert!(false, "BackgroundTask scheduled during initialize() — Harness has no executor; task dropped silently");
    }

    fn set_latency_samples(&self, _samples: u32) {}

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

/// A minimal [`ProcessContext`] for headless driving. Carries the transport for
/// this `process()` call and a caller-supplied slice of input note events; any
/// events the plugin emits are collected into `output_events` for the caller to
/// inspect afterwards.
struct HarnessProcessContext<'a, P: Plugin> {
    transport: Transport,
    input_events: &'a [PluginNoteEvent<P>],
    input_events_idx: usize,
    output_events: &'a mut Vec<PluginNoteEvent<P>>,
}

impl<P: Plugin> ProcessContext<P> for HarnessProcessContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Standalone
    }

    fn execute_background(&self, _task: P::BackgroundTask) {
        debug_assert!(false, "BackgroundTask scheduled during process() — Harness has no executor; task dropped silently");
    }

    fn execute_gui(&self, _task: P::BackgroundTask) {
        debug_assert!(false, "GUI task scheduled during process() — Harness has no GUI; task dropped silently");
    }

    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        if self.input_events_idx < self.input_events.len() {
            let event = self.input_events[self.input_events_idx].clone();
            self.input_events_idx += 1;
            Some(event)
        } else {
            None
        }
    }

    fn send_event(&mut self, event: PluginNoteEvent<P>) {
        self.output_events.push(event);
    }

    fn set_latency_samples(&self, _samples: u32) {}

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

/// Collected observations from a [`ScenarioBuilder`] run — one entry per
/// `.step()` call.
pub struct ScenarioResult<P: Plugin> {
    /// [`ProcessStatus`] returned by `process()` for each step, in order.
    pub statuses: Vec<ProcessStatus>,
    /// All [`PluginNoteEvent`]s emitted across every step, in emission order.
    pub output_events: Vec<PluginNoteEvent<P>>,
    /// Per-step snapshot of `main_io` after each `process()` call.
    ///
    /// `output_buffers[i][ch][sample]` — outer index is step, then channel,
    /// then sample within that channel.
    pub output_buffers: Vec<Vec<Vec<f32>>>,
}

/// Fluent builder for multi-step scenarios. Obtained via [`Harness::scenario`].
///
/// Each `.step()` drives one `process()` call with the current transport and
/// MIDI inputs, then clears the per-step MIDI queue (matching how a real host
/// delivers events per-period). Accumulated results are returned by `.finish()`.
///
/// Transport position fields on [`Transport`] are `pub(crate)` inside nih-plug
/// and cannot be mutated by this crate. If position matters for a test, supply
/// a fresh [`Transport`] with the desired position via `.transport()` before
/// each `.step()`.
pub struct ScenarioBuilder<'h, P: Plugin> {
    harness: &'h mut Harness<P>,
    buffer_size: usize,
    transport: Transport,
    /// Per-step MIDI events — cleared after each `.step()` consumes them.
    pending_midi: Vec<PluginNoteEvent<P>>,
    result: ScenarioResult<P>,
}

impl<'h, P: Plugin> ScenarioBuilder<'h, P> {
    /// Create a new builder borrowing `harness`, with an initial transport at
    /// position zero and the given `buffer_size` (samples per step).
    pub fn new(harness: &'h mut Harness<P>, buffer_size: usize) -> Self {
        let transport = harness.new_transport();
        Self {
            harness,
            buffer_size,
            transport,
            pending_midi: Vec::new(),
            result: ScenarioResult {
                statuses: Vec::new(),
                output_events: Vec::new(),
                output_buffers: Vec::new(),
            },
        }
    }

    /// Apply a mutation directly to the plugin (param updates, flag flips,
    /// any test-only state change).
    pub fn modify_plugin(self, f: impl FnOnce(&mut P)) -> Self {
        f(&mut self.harness.plugin);
        self
    }

    /// Replace the transport used for subsequent `.step()` calls.
    pub fn transport(mut self, t: Transport) -> Self {
        self.transport = t;
        self
    }

    /// Queue MIDI events to be delivered on the *next* `.step()` only.
    /// Cleared automatically after that step consumes them.
    pub fn midi_in(mut self, events: Vec<PluginNoteEvent<P>>) -> Self {
        self.pending_midi = events;
        self
    }

    /// Set the buffer length (number of samples) for subsequent `.step()` calls.
    pub fn buffer_size(mut self, n: usize) -> Self {
        self.buffer_size = n;
        self
    }

    /// Drive one `process()` call with the current configuration.
    ///
    /// After the call:
    /// - Pending MIDI queue is cleared (consumed for this step only).
    /// - `main_io` snapshot is appended to `result.output_buffers`.
    ///
    /// The transport is consumed for this step. The builder retains a fresh
    /// default transport (`harness.new_transport()`) — call `.transport()` before
    /// the next `.step()` if the next step needs specific transport settings.
    pub fn step(mut self) -> Self {
        let num_channels = self
            .harness
            .audio_io_layout
            .main_output_channels
            .map(|n| n.get() as usize)
            .unwrap_or(2);

        let mut main_io: Vec<Vec<f32>> = vec![vec![0.0f32; self.buffer_size]; num_channels];

        // Replace self.transport with a fresh default so self remains valid after the move.
        // Must split the borrow: get the placeholder first (read-only borrow), then replace.
        let placeholder = self.harness.new_transport();
        let step_transport = std::mem::replace(&mut self.transport, placeholder);
        let midi: Vec<PluginNoteEvent<P>> = std::mem::take(&mut self.pending_midi);
        let (status, out_events) = self.harness.process(&mut main_io, step_transport, &midi);

        // Snapshot output buffer (cloned so the caller owns it).
        self.result.output_buffers.push(main_io);
        self.result.statuses.push(status);
        self.result.output_events.extend(out_events);

        self
    }

    /// Consume the builder and return accumulated results.
    pub fn finish(self) -> ScenarioResult<P> {
        self.result
    }
}

impl<P: Plugin> Harness<P> {
    /// Construct and initialize a fresh `P`, mirroring
    /// `Wrapper::new()` (`wrapper.rs:184-300`): `P::default()` → pick the
    /// plugin's default (first-listed) [`AudioIOLayout`] → `initialize()` →
    /// `reset()`.
    ///
    /// Returns `None` if the plugin declares no audio IO layouts, or if
    /// `initialize()` reports failure (matching `Wrapper::new`'s
    /// `WrapperError::InitializationFailed` path).
    pub fn new(sample_rate: f32, max_buffer_size: u32) -> Option<Self> {
        let audio_io_layout = *P::AUDIO_IO_LAYOUTS.first()?;

        let mut plugin = P::default();

        let buffer_config = BufferConfig {
            sample_rate,
            min_buffer_size: None,
            max_buffer_size,
            process_mode: ProcessMode::Realtime,
        };

        let mut init_context = HarnessInitContext;
        if !plugin.initialize(&audio_io_layout, &buffer_config, &mut init_context) {
            return None;
        }
        plugin.reset();

        Some(Self {
            plugin,
            audio_io_layout,
            sample_rate,
        })
    }

    /// Drive one `process()` call, mirroring the standalone backend's per-period
    /// call (`wrapper.rs:521-536`): build [`Buffer`]s from the caller's raw
    /// per-channel storage, assemble a [`Transport`] from `transport`, run
    /// `process()`, and return its [`ProcessStatus`] plus any note events the
    /// plugin emitted.
    ///
    /// `main_io` holds one `Vec<f32>` per main-output channel — processed
    /// in-place, matching nih-plug's in-place buffer convention. Auxiliary
    /// buffers are not modeled in this checkpoint; an empty [`AuxiliaryBuffers`]
    /// is passed (the plugin sees zero aux ports either way unless its layout
    /// declares them, in which case this harness presents empty slices for
    /// those ports — sufficient for the smoke test's scope).
    pub fn process(
        &mut self,
        main_io: &mut [Vec<f32>],
        transport: Transport,
        input_events: &[PluginNoteEvent<P>],
    ) -> (ProcessStatus, Vec<PluginNoteEvent<P>>) {
        let num_samples = main_io.first().map(|c| c.len()).unwrap_or(0);

        let mut buffer = Buffer::default();
        // SAFETY: `main_io`'s channel `Vec`s outlive this call (borrowed for
        // its duration), each has length `num_samples`, matching the contract
        // documented on `Buffer::set_slices`.
        unsafe {
            buffer.set_slices(num_samples, |slices| {
                slices.clear();
                slices.extend(main_io.iter_mut().map(|channel| channel.as_mut_slice()));
            });
        }

        let mut aux_inputs: Vec<Buffer> = Vec::new();
        let mut aux_outputs: Vec<Buffer> = Vec::new();
        let mut aux = AuxiliaryBuffers {
            inputs: &mut aux_inputs,
            outputs: &mut aux_outputs,
        };

        let mut output_events = Vec::new();
        let mut context = HarnessProcessContext::<P> {
            transport,
            input_events,
            input_events_idx: 0,
            output_events: &mut output_events,
        };

        let status = self.plugin.process(&mut buffer, &mut aux, &mut context);

        (status, output_events)
    }

    /// Tear the plugin down, mirroring the lifecycle contract documented on
    /// [`Plugin::deactivate`]: call it, then `reset()` so a subsequent
    /// `initialize()` (were the caller to build a new [`Harness`]) starts from
    /// clean state. `nih_export_standalone` itself never calls `deactivate()`
    /// explicitly (the plugin simply drops on exit), but exposing it here lets
    /// scripted tests exercise the same teardown path a real host would use
    /// when swapping configurations.
    pub fn deactivate(mut self) {
        self.plugin.deactivate();
        self.plugin.reset();
    }

    /// A fresh [`Transport`] for this harness's sample rate, with no other
    /// fields populated — the same starting point `Transport::new()` gives
    /// `Wrapper::new`'s callers (`context/process.rs:164`).
    pub fn new_transport(&self) -> Transport {
        Transport::new(self.sample_rate)
    }

    /// The audio IO layout this harness initialized the plugin with.
    pub fn audio_io_layout(&self) -> &AudioIOLayout {
        &self.audio_io_layout
    }

    /// Read-only access to the plugin for test assertions.
    pub fn plugin(&self) -> &P {
        &self.plugin
    }

    /// Mutable access to the plugin for test-only state inspection or mutation.
    pub fn plugin_mut(&mut self) -> &mut P {
        &mut self.plugin
    }

    /// Shorthand for [`ScenarioBuilder::new`]: borrow this harness and begin
    /// assembling a multi-step scenario with the given initial buffer size.
    pub fn scenario(&mut self, buffer_size: usize) -> ScenarioBuilder<'_, P> {
        ScenarioBuilder::new(self, buffer_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nih_plug::prelude::*;
    use std::sync::Arc;

    /// Trivial mock plugin: one stereo in/out port, no params, no MIDI. Its
    /// `process()` writes a recognizable constant into every output sample so
    /// the test can observe that the harness actually drove a real `process()`
    /// call through to the plugin (not a stub).
    #[derive(Default)]
    struct MockPlugin;

    const MOCK_AUDIO_IO_LAYOUTS: &[AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];

    struct MockParams;
    unsafe impl Params for MockParams {
        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
            Vec::new()
        }
    }

    impl Plugin for MockPlugin {
        const NAME: &'static str = "Mock";
        const VENDOR: &'static str = "EtherTap tests";
        const URL: &'static str = "https://example.invalid";
        const EMAIL: &'static str = "mock@example.invalid";
        const VERSION: &'static str = "0.0.0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = MOCK_AUDIO_IO_LAYOUTS;

        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            Arc::new(MockParams)
        }

        fn process(
            &mut self,
            buffer: &mut Buffer,
            _aux: &mut AuxiliaryBuffers,
            _context: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            for channel_samples in buffer.iter_samples() {
                for sample in channel_samples {
                    *sample = 0.5;
                }
            }

            ProcessStatus::Normal
        }
    }

    /// A richer mock plugin for scenario testing: counts how many times
    /// `process()` has been called and echoes received MIDI events as output
    /// events, so tests can assert on `output_events` and plugin state.
    #[derive(Default)]
    struct CountingPlugin {
        process_count: u32,
        /// Set by `.modify_plugin()` in tests to vary output values.
        output_value: f32,
    }

    impl Plugin for CountingPlugin {
        const NAME: &'static str = "Counting";
        const VENDOR: &'static str = "EtherTap tests";
        const URL: &'static str = "https://example.invalid";
        const EMAIL: &'static str = "mock@example.invalid";
        const VERSION: &'static str = "0.0.0";
        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = MOCK_AUDIO_IO_LAYOUTS;

        type SysExMessage = ();
        type BackgroundTask = ();

        fn params(&self) -> Arc<dyn Params> {
            Arc::new(MockParams)
        }

        fn process(
            &mut self,
            buffer: &mut Buffer,
            _aux: &mut AuxiliaryBuffers,
            context: &mut impl ProcessContext<Self>,
        ) -> ProcessStatus {
            self.process_count += 1;
            let val = self.output_value;
            for channel_samples in buffer.iter_samples() {
                for sample in channel_samples {
                    *sample = val;
                }
            }
            // Echo received MIDI events back out.
            while let Some(event) = context.next_event() {
                context.send_event(event);
            }
            ProcessStatus::Normal
        }
    }

    #[test]
    fn scenario_multi_step_api() {
        let mut harness = Harness::<CountingPlugin>::new(44_100.0, 512)
            .expect("counting plugin should initialize");

        // Confirm plugin() accessor works before the scenario.
        assert_eq!(harness.plugin().process_count, 0);

        let mut transport = harness.new_transport();
        transport.playing = true;
        transport.tempo = Some(120.0);

        // Build a NoteOn event to send on step 1.
        let note_on = PluginNoteEvent::<CountingPlugin>::NoteOn {
            timing: 0,
            voice_id: None,
            channel: 0,
            note: 60,
            velocity: 1.0,
        };

        let result = harness
            .scenario(128)
            // Step 1: set output_value to 0.25 via modify_plugin, send a NoteOn.
            .modify_plugin(|p| p.output_value = 0.25)
            .transport(transport)
            .midi_in(vec![note_on])
            .step()
            // Step 2: different output_value, no MIDI (auto-cleared), larger buffer.
            .modify_plugin(|p| p.output_value = 0.75)
            .buffer_size(256)
            .step()
            .finish();

        // Two steps → two statuses.
        assert_eq!(result.statuses.len(), 2);
        assert!(result.statuses.iter().all(|&s| s == ProcessStatus::Normal));

        // Two buffer snapshots.
        assert_eq!(result.output_buffers.len(), 2);
        // Step 1: 2 channels × 128 samples at 0.25.
        let step1 = &result.output_buffers[0];
        assert_eq!(step1.len(), 2);
        assert_eq!(step1[0].len(), 128);
        assert!(step1[0].iter().all(|&s| s == 0.25));
        // Step 2: 2 channels × 256 samples at 0.75.
        let step2 = &result.output_buffers[1];
        assert_eq!(step2.len(), 2);
        assert_eq!(step2[0].len(), 256);
        assert!(step2[0].iter().all(|&s| s == 0.75));

        // The NoteOn sent on step 1 should appear in output_events (echoed back).
        assert_eq!(result.output_events.len(), 1);
        match result.output_events[0] {
            PluginNoteEvent::<CountingPlugin>::NoteOn { note, channel, .. } => {
                assert_eq!(note, 60);
                assert_eq!(channel, 0);
            }
            _ => panic!("expected NoteOn in output_events"),
        }

        // plugin() accessor: process_count should be 2 after two steps.
        assert_eq!(harness.plugin().process_count, 2);

        // plugin_mut() accessor: can reset the counter directly.
        harness.plugin_mut().process_count = 0;
        assert_eq!(harness.plugin().process_count, 0);

        harness.deactivate();
    }

    #[test]
    fn drives_a_mock_plugin_through_one_process_call() {
        let mut harness =
            Harness::<MockPlugin>::new(44_100.0, 256).expect("mock plugin should initialize");

        let mut main_io = vec![vec![0.0f32; 128], vec![0.0f32; 128]];
        let transport = harness.new_transport();

        let (status, output_events) = harness.process(&mut main_io, transport, &[]);

        assert_eq!(status, ProcessStatus::Normal);
        assert!(output_events.is_empty());
        for channel in &main_io {
            for &sample in channel {
                assert_eq!(sample, 0.5, "process() should have written through to the buffer");
            }
        }

        harness.deactivate();
    }
}
