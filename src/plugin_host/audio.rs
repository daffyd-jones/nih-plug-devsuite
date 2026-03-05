#![allow(unsafe_code)]

use crate::plugin_host::handlers::DevHost;
use crate::plugin_host::midi_bridge::{MidiBridge, RawMidiEvent};
use crate::plugin_host::PluginMode;

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortType, PluginAudioPorts,
};
use clack_host::prelude::*;
use cpal::FromSample;
use cpal::Sample;
use rtrb::Consumer;

// ── Port Configuration ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct PortConfig {
    pub ports: Vec<PortInfo>,
    pub main_port_index: u32,
}

#[derive(Clone, Debug)]
pub struct PortInfo {
    pub channel_count: u16,
    pub name: String,
}

impl PortConfig {
    fn empty() -> Self {
        Self {
            ports: vec![],
            main_port_index: 0,
        }
    }

    fn default_stereo() -> Self {
        Self {
            ports: vec![PortInfo {
                channel_count: 2,
                name: "Default".into(),
            }],
            main_port_index: 0,
        }
    }

    pub fn main_port(&self) -> &PortInfo {
        &self.ports[self.main_port_index as usize]
    }

    pub fn total_channel_count(&self) -> usize {
        self.ports.iter().map(|p| p.channel_count as usize).sum()
    }
}

fn query_ports(plugin: &mut PluginMainThreadHandle, is_input: bool) -> PortConfig {
    let Some(audio_ports) = plugin.get_extension::<PluginAudioPorts>() else {
        return if is_input {
            PortConfig::empty()
        } else {
            PortConfig::default_stereo()
        };
    };

    let mut buffer = AudioPortInfoBuffer::new();
    let mut ports = vec![];
    let mut main_idx = None;

    for i in 0..audio_ports.count(plugin, is_input) {
        let Some(info) = audio_ports.get(plugin, i, is_input, &mut buffer) else {
            continue;
        };

        let port_type = info
            .port_type
            .or_else(|| AudioPortType::from_channel_count(info.channel_count));

        let channel_count = match port_type {
            Some(t) if t == AudioPortType::MONO => 1,
            Some(t) if t == AudioPortType::STEREO => 2,
            _ => info.channel_count as u16,
        };

        if info.flags.contains(AudioPortFlags::IS_MAIN) {
            main_idx = Some(i);
        }

        ports.push(PortInfo {
            channel_count,
            name: String::from_utf8_lossy(info.name).into_owned(),
        });
    }

    if ports.is_empty() {
        return if is_input {
            PortConfig::empty()
        } else {
            PortConfig::default_stereo()
        };
    }

    PortConfig {
        main_port_index: main_idx.unwrap_or(0),
        ports,
    }
}

// ── Audio Config ─────────────────────────────────────────────────────────────

pub struct PluginAudioConfig {
    pub sample_rate: u32,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub mode: PluginMode,
}

// ── Plugin Audio Processor (lives on audio thread) ───────────────────────────

/// This struct is `Send` so it can be moved to the cpal audio thread.
/// It holds a `StoppedPluginAudioProcessor` that will be started on first process call.
pub struct PluginAudioProcessor {
    /// Initially Some — consumed on first process() call
    stopped: Option<StoppedPluginAudioProcessor<DevHost>>,
    /// Populated after start_processing() succeeds
    started: Option<StartedPluginAudioProcessor<DevHost>>,

    midi_bridge: MidiBridge,

    input_ports: AudioPorts,
    output_ports: AudioPorts,
    input_port_channels: Box<[Vec<f32>]>,
    output_port_channels: Box<[Vec<f32>]>,
    muxed: Vec<f32>,

    output_channel_count: usize,
    input_port_config: PortConfig,
    output_port_config: PortConfig,
    frame_capacity: usize,

    steady_counter: u64,
    mode: PluginMode,
}

// SAFETY: StoppedPluginAudioProcessor is Send. Once we call start_processing()
// on the audio thread, the StartedPluginAudioProcessor stays there forever.
unsafe impl Send for PluginAudioProcessor {}

impl PluginAudioProcessor {
    pub fn new(
        instance: &mut PluginInstance<DevHost>,
        midi_consumer: Consumer<RawMidiEvent>,
        config: PluginAudioConfig,
    ) -> Result<Self, String> {
        let input_port_config = query_ports(&mut instance.plugin_handle(), true);
        let output_port_config = query_ports(&mut instance.plugin_handle(), false);

        let midi_bridge = MidiBridge::new(midi_consumer, instance);

        let plugin_config = PluginAudioConfiguration {
            sample_rate: config.sample_rate as f64,
            min_frames_count: config.min_buffer_size,
            max_frames_count: config.max_buffer_size,
        };

        let stopped = instance
            .activate(|_, _| (), plugin_config)
            .map_err(|e| format!("Failed to activate plugin: {e}"))?;

        let frame_capacity = config.max_buffer_size as usize;

        let output_channel_count = if output_port_config.ports.is_empty() {
            2
        } else {
            output_port_config.main_port().channel_count as usize
        };

        let total_in_channels = input_port_config.total_channel_count();
        let total_out_channels = output_port_config.total_channel_count();

        let input_port_channels: Box<[Vec<f32>]> = input_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_capacity * p.channel_count as usize])
            .collect();

        let output_port_channels: Box<[Vec<f32>]> = output_port_config
            .ports
            .iter()
            .map(|p| vec![0.0f32; frame_capacity * p.channel_count as usize])
            .collect();

        let muxed = vec![0.0f32; frame_capacity * output_channel_count.max(2)];

        Ok(Self {
            stopped: Some(stopped),
            started: None,
            midi_bridge,
            input_ports: AudioPorts::with_capacity(
                total_in_channels,
                input_port_config.ports.len(),
            ),
            output_ports: AudioPorts::with_capacity(
                total_out_channels,
                output_port_config.ports.len(),
            ),
            input_port_channels,
            output_port_channels,
            muxed,
            output_channel_count: output_channel_count.max(1),
            input_port_config,
            output_port_config,
            frame_capacity,
            steady_counter: 0,
            mode: config.mode,
        })
    }

    /// Returns the number of output channels the plugin will produce.
    pub fn output_channel_count(&self) -> usize {
        self.output_channel_count
    }

    /// Process audio. Called from the cpal output callback.
    /// `input_data` may be empty for instrument mode.
    /// Writes interleaved output into `output_data`.
    pub fn process<S: FromSample<f32>>(&mut self, input_data: &[f32], output_data: &mut [S]) {
        // Start processing on first call
        if self.started.is_none() {
            if let Some(stopped) = self.stopped.take() {
                match stopped.start_processing() {
                    Ok(started) => self.started = Some(started),
                    Err(e) => {
                        eprintln!("[plugin_audio] Failed to start processing: {e}");
                        output_data
                            .iter_mut()
                            .for_each(|s| *s = f32::to_sample(0.0));
                        return;
                    }
                }
            }
        }

        let cpal_frames = output_data.len() / self.output_channel_count.max(1);
        let frame_count = cpal_frames.min(self.frame_capacity);

        if frame_count == 0 {
            return;
        }

        // Ensure buffers are large enough and clear them
        self.ensure_capacity(frame_count);

        // Copy input audio (de-interleave) for effect mode
        if self.mode == PluginMode::Effect
            && !input_data.is_empty()
            && !self.input_port_config.ports.is_empty()
        {
            let main_idx = self.input_port_config.main_port_index as usize;
            let in_ch_count = self.input_port_config.main_port().channel_count as usize;
            let buf = &mut self.input_port_channels[main_idx];

            // Calculate actual input channels available
            let input_channels = input_data.len() / frame_count;
            let channels_to_copy = in_ch_count.min(input_channels).min(buf.len() / self.frame_capacity);

            for frame in 0..frame_count {
                for ch in 0..channels_to_copy {
                    let interleaved_idx = frame * input_channels + ch;
                    let deinterleaved_idx = ch * self.frame_capacity + frame;
                    if interleaved_idx < input_data.len() && deinterleaved_idx < buf.len() {
                        buf[deinterleaved_idx] = input_data[interleaved_idx];
                    }
                }
            }
        }

        // Prepare CLAP buffers and get MIDI events in the same scope
        let (ins, mut outs) = {
            let cap = self.frame_capacity;

            let inputs = self
                .input_ports
                .with_input_buffers(self.input_port_channels.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            port_buf.chunks_exact_mut(cap).map(|buffer| InputChannel {
                                buffer: &mut buffer[..frame_count],
                                is_constant: false,
                            }),
                        ),
                    }
                }));

            let outputs = self
                .output_ports
                .with_output_buffers(self.output_port_channels.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(
                            port_buf
                                .chunks_exact_mut(cap)
                                .map(|buf| &mut buf[..frame_count]),
                        ),
                    }
                }));

            (inputs, outputs)
        };

        let events = self.midi_bridge.drain_to_input_events(frame_count as u32);
        let steady_counter = self.steady_counter;
        let event_count = events.len();

        // Now get the processor after all other borrows are done
        let Some(ref mut processor) = self.started else {
            output_data
                .iter_mut()
                .for_each(|s| *s = f32::to_sample(0.0));
            return;
        };

        // Additional safety check to prevent index out of bounds
        if frame_count > self.frame_capacity {
            eprintln!("[plugin_audio] Frame count {} exceeds capacity {}", frame_count, self.frame_capacity);
            output_data.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            return;
        }

        // Process
        match processor.process(
            &ins,
            &mut outs,
            &events,
            &mut OutputEvents::void(),
            Some(steady_counter),
            None,
        ) {
            Ok(_) => {
                // Check if plugin produced any output
                let mut has_output = false;
                for (i, buf) in self.output_port_channels.iter().enumerate() {
                    let sum: f32 = buf.iter().take(frame_count).sum();
                    if sum.abs() > 0.001 {
                        eprintln!("[plugin_audio] Port {} produced non-zero output: {}", i, sum);
                        has_output = true;
                    }
                }
                if !has_output && event_count > 0 {
                    eprintln!("[plugin_audio] Plugin processed {} events but produced no audio output", event_count);
                }
            }
            Err(e) => {
                eprintln!("[plugin_audio] Process error: {e}");
                output_data
                    .iter_mut()
                    .for_each(|s| *s = f32::to_sample(0.0));
                return;
            }
        }

        self.steady_counter += frame_count as u64;

        // Interleave output
        self.write_output(output_data, frame_count);
    }

    fn ensure_capacity(&mut self, needed: usize) {
        if needed <= self.frame_capacity {
            // Clear existing buffers even if no resize needed
            for buf in &mut self.output_port_channels {
                buf.fill(0.0);
            }
            for buf in &mut self.input_port_channels {
                buf.fill(0.0);
            }
            return;
        }

        self.frame_capacity = needed;

        for (buf, port) in self
            .input_port_channels
            .iter_mut()
            .zip(&self.input_port_config.ports)
        {
            buf.resize(needed * port.channel_count as usize, 0.0);
        }
        for (buf, port) in self
            .output_port_channels
            .iter_mut()
            .zip(&self.output_port_config.ports)
        {
            buf.resize(needed * port.channel_count as usize, 0.0);
        }
        self.muxed.resize(needed * self.output_channel_count, 0.0);
    }

    fn write_output<S: FromSample<f32>>(&mut self, output: &mut [S], frame_count: usize) {
        if self.output_port_config.ports.is_empty() {
            output.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            return;
        }

        let main_idx = self.output_port_config.main_port_index as usize;
        let main_buf = &self.output_port_channels[main_idx];
        let plugin_ch = self.output_port_config.main_port().channel_count as usize;
        let out_ch = self.output_channel_count;

        // Safety check for buffer sizes
        if main_buf.len() < self.frame_capacity * plugin_ch {
            eprintln!("[plugin_audio] Output buffer too small: {} < {}", main_buf.len(), self.frame_capacity * plugin_ch);
            output.iter_mut().for_each(|s| *s = f32::to_sample(0.0));
            return;
        }

        // Check if we have actual audio data to output
        let mut output_sum = 0.0f32;
        let mut silent_samples = 0;

        // Interleave from de-interleaved plugin output into cpal's interleaved buffer
        for frame in 0..frame_count {
            for ch in 0..out_ch {
                let out_idx = frame * out_ch + ch;
                if out_idx >= output.len() {
                    break;
                }

                let src_ch = if ch < plugin_ch { ch } else { 0 }; // mono→stereo duplication
                let src_idx = src_ch * self.frame_capacity + frame;

                let sample = if src_idx < main_buf.len() {
                    main_buf[src_idx]
                } else {
                    0.0
                };

                output_sum += sample.abs();
                if sample.abs() < 0.0001 {
                    silent_samples += 1;
                }

                output[out_idx] = sample.to_sample();
            }
        }

        // Log output statistics
        if output_sum > 0.1 {
            eprintln!("[plugin_audio] Final output: sum={:.3}, silent_samples={}/{}", output_sum, silent_samples, frame_count * out_ch);
        }
    }
}
