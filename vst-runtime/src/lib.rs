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
    PluginNoteEvent, ProcessContext, ProcessMode, ProcessStatus, Transport,
};

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
        // The harness does not run a background task executor; tasks scheduled
        // here are silently dropped, mirroring "no host-side queue" rather than
        // panicking on a code path real plugins rarely exercise during init.
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

    fn execute_background(&self, _task: P::BackgroundTask) {}

    fn execute_gui(&self, _task: P::BackgroundTask) {}

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
