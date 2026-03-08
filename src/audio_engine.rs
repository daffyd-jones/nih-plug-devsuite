use crate::plugin_host::audio::PluginAudioProcessor;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const COMMON_SAMPLE_RATES: &[u32] = &[44100, 48000, 88200, 96000, 192000];

pub const BUFFER_SIZE_OPTIONS: &[(&str, Option<u32>)] = &[
    ("Default", Some(256)),
    ("64", Some(64)),
    ("128", Some(128)),
    ("256", Some(256)),
    ("512", Some(512)),
    ("1024", Some(1024)),
    ("2048", Some(2048)),
];

#[derive(Debug, Clone, PartialEq)]
pub enum AudioStatus {
    Stopped,
    Running,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct RunningInfo {
    pub input_device: String,
    pub output_device: String,
    pub sample_rate: u32,
    pub buffer_size: String,
    pub channels: u16,
}

pub struct AudioEngine {
    pub status: AudioStatus,
    pub running_info: Option<RunningInfo>,

    pub input_device_names: Vec<String>,
    pub output_device_names: Vec<String>,

    pub selected_input_idx: usize,
    pub selected_output_idx: usize,
    pub selected_sample_rate_idx: usize,
    pub selected_buffer_size_idx: usize,

    _streams: Option<StreamHolder>,
}

enum StreamHolder {
    Passthrough(Stream, Stream),
    PluginOutput(Stream),
    PluginInsert(Stream, Stream),
}

impl AudioEngine {
    pub fn new() -> Self {
        let (inputs, outputs) = enumerate_devices();
        Self {
            status: AudioStatus::Stopped,
            running_info: None,
            input_device_names: inputs,
            output_device_names: outputs,
            selected_input_idx: 0,
            selected_output_idx: 0,
            selected_sample_rate_idx: 0,
            selected_buffer_size_idx: 0,
            _streams: None,
        }
    }

    pub fn refresh_devices(&mut self) {
        let (inputs, outputs) = enumerate_devices();
        self.input_device_names = inputs;
        self.output_device_names = outputs;
        if !self.input_device_names.is_empty() {
            self.selected_input_idx = self
                .selected_input_idx
                .min(self.input_device_names.len() - 1);
        }
        if !self.output_device_names.is_empty() {
            self.selected_output_idx = self
                .selected_output_idx
                .min(self.output_device_names.len() - 1);
        }
    }

    pub fn current_sample_rate(&self) -> u32 {
        COMMON_SAMPLE_RATES[self.selected_sample_rate_idx]
    }

    pub fn current_buffer_size(&self) -> (u32, u32) {
        match BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx].1 {
            Some(n) => (n, n),
            None => (256, 1024),
        }
    }

    /// Start passthrough (no plugin).
    pub fn start(&mut self) {
        self.stop();
        match build_passthrough_streams(
            self.selected_input_idx,
            &self.input_device_names,
            self.selected_output_idx,
            &self.output_device_names,
            COMMON_SAMPLE_RATES[self.selected_sample_rate_idx],
            BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx],
        ) {
            Ok((in_stream, out_stream, info)) => {
                let _ = in_stream.play();
                let _ = out_stream.play();
                self._streams = Some(StreamHolder::Passthrough(in_stream, out_stream));
                self.running_info = Some(info);
                self.status = AudioStatus::Running;
            }
            Err(e) => {
                self.status = AudioStatus::Error(e);
                self.running_info = None;
            }
        }
    }

    /// Start with a plugin processor (instrument mode — output only, MIDI → audio).
    pub fn start_with_plugin_instrument(&mut self, processor: PluginAudioProcessor) {
        self.stop();

        let host = cpal::default_host();
        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];

        let output_device =
            match get_output_device(&host, self.selected_output_idx, &self.output_device_names) {
                Ok(d) => d,
                Err(e) => {
                    self.status = AudioStatus::Error(e);
                    return;
                }
            };

        let output_name = output_device
            .name()
            .unwrap_or_else(|_| "Unknown".to_string());

        let out_ch = processor.output_channel_count().max(1).min(2) as u16;

        let buffer_size = match buffer_opt.1 {
            Some(n) => {
                println!("[audio] Instrument mode - Using fixed buffer size: {}", n);
                BufferSize::Fixed(n)
            }
            None => {
                println!("[audio] Instrument mode - Using default buffer size");
                BufferSize::Default
            }
        };

        let config = StreamConfig {
            channels: out_ch,
            sample_rate: SampleRate(sample_rate),
            buffer_size,
        };

        println!(
            "[audio] Instrument stream config: {} channels, {}Hz, {:?}",
            out_ch, sample_rate, config.buffer_size
        );

        let processor = Arc::new(Mutex::new(processor));
        let proc_clone = Arc::clone(&processor);

        let out_stream = match output_device.build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                if let Ok(mut proc) = proc_clone.try_lock() {
                    let empty_input: &[f32] = &[];
                    proc.process::<f32>(empty_input, data);
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.status = AudioStatus::Error(format!("Failed to build output stream: {e}"));
                return;
            }
        };

        if let Err(e) = out_stream.play() {
            self.status = AudioStatus::Error(format!("Failed to play output stream: {e}"));
            return;
        }

        self.running_info = Some(RunningInfo {
            input_device: "(none — instrument mode)".into(),
            output_device: output_name,
            sample_rate,
            buffer_size: buffer_opt.0.to_string(),
            channels: out_ch,
        });
        self._streams = Some(StreamHolder::PluginOutput(out_stream));
        self.status = AudioStatus::Running;
    }

    /// Start with a plugin processor (effect mode — audio in → plugin → audio out).
    pub fn start_with_plugin_effect(&mut self, processor: PluginAudioProcessor) {
        self.stop();

        let host = cpal::default_host();
        let sample_rate = COMMON_SAMPLE_RATES[self.selected_sample_rate_idx];
        let buffer_opt = BUFFER_SIZE_OPTIONS[self.selected_buffer_size_idx];

        let input_device =
            match get_input_device(&host, self.selected_input_idx, &self.input_device_names) {
                Ok(d) => d,
                Err(e) => {
                    self.status = AudioStatus::Error(e);
                    return;
                }
            };

        let output_device =
            match get_output_device(&host, self.selected_output_idx, &self.output_device_names) {
                Ok(d) => d,
                Err(e) => {
                    self.status = AudioStatus::Error(e);
                    return;
                }
            };

        let input_name = input_device
            .name()
            .unwrap_or_else(|_| "Unknown".to_string());
        let output_name = output_device
            .name()
            .unwrap_or_else(|_| "Unknown".to_string());

        let out_ch = processor.output_channel_count().max(1).min(2) as u16;

        let buffer_size = match buffer_opt.1 {
            Some(n) => BufferSize::Fixed(n),
            None => BufferSize::Default,
        };

        let config = StreamConfig {
            channels: out_ch,
            sample_rate: SampleRate(sample_rate),
            buffer_size,
        };

        // Ring buffer for input → output routing
        let capacity = (sample_rate as usize * out_ch as usize).max(65_536);
        let shared: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let shared_in = Arc::clone(&shared);
        let shared_out = Arc::clone(&shared);
        let max_fill = capacity;

        let in_stream = match input_device.build_input_stream(
            &config,
            move |data: &[f32], _info| {
                if let Ok(mut buf) = shared_in.try_lock() {
                    for &s in data {
                        if buf.len() < max_fill {
                            buf.push_back(s);
                        }
                    }
                }
            },
            |err| eprintln!("[audio] input error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.status = AudioStatus::Error(format!("Failed to build input stream: {e}"));
                return;
            }
        };

        let processor = Arc::new(Mutex::new(processor));
        let proc_clone = Arc::clone(&processor);
        let ch_count = out_ch as usize;

        let out_stream = match output_device.build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                // Collect input samples
                let frame_count = data.len();
                let mut input_buf = vec![0.0f32; frame_count];

                if let Ok(mut ring) = shared_out.try_lock() {
                    for s in input_buf.iter_mut() {
                        *s = ring.pop_front().unwrap_or(0.0);
                    }
                }

                if let Ok(mut proc) = proc_clone.try_lock() {
                    proc.process::<f32>(&input_buf, data);
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.status = AudioStatus::Error(format!("Failed to build output stream: {e}"));
                return;
            }
        };

        let _ = in_stream.play();
        let _ = out_stream.play();

        self.running_info = Some(RunningInfo {
            input_device: input_name,
            output_device: output_name,
            sample_rate,
            buffer_size: buffer_opt.0.to_string(),
            channels: out_ch,
        });
        self._streams = Some(StreamHolder::PluginInsert(in_stream, out_stream));
        self.status = AudioStatus::Running;
    }

    pub fn stop(&mut self) {
        self._streams = None;
        self.status = AudioStatus::Stopped;
        self.running_info = None;
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn enumerate_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();

    let mut inputs = vec!["Default".to_string()];
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                inputs.push(name);
            }
        }
    }

    let mut outputs = vec!["Default".to_string()];
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                outputs.push(name);
            }
        }
    }

    (inputs, outputs)
}

fn get_input_device(host: &cpal::Host, idx: usize, names: &[String]) -> Result<Device, String> {
    if idx == 0 {
        host.default_input_device()
            .ok_or_else(|| "No default input device found".to_string())
    } else {
        let target = &names[idx];
        host.input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map_or(false, |n| n == *target))
            .ok_or_else(|| format!("Input device not found: {target}"))
    }
}

fn get_output_device(host: &cpal::Host, idx: usize, names: &[String]) -> Result<Device, String> {
    if idx == 0 {
        host.default_output_device()
            .ok_or_else(|| "No default output device found".to_string())
    } else {
        let target = &names[idx];
        host.output_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map_or(false, |n| n == *target))
            .ok_or_else(|| format!("Output device not found: {target}"))
    }
}

fn build_passthrough_streams(
    in_idx: usize,
    in_names: &[String],
    out_idx: usize,
    out_names: &[String],
    sample_rate: u32,
    buffer_opt: (&str, Option<u32>),
) -> Result<(Stream, Stream, RunningInfo), String> {
    let host = cpal::default_host();

    let input_device = get_input_device(&host, in_idx, in_names)?;
    let output_device = get_output_device(&host, out_idx, out_names)?;

    let input_name = input_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());
    let output_name = output_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    let in_default = input_device
        .default_input_config()
        .map_err(|e| format!("Input config error: {e}"))?;
    let out_default = output_device
        .default_output_config()
        .map_err(|e| format!("Output config error: {e}"))?;
    let channels = in_default.channels().min(out_default.channels());

    let buffer_size = match buffer_opt.1 {
        Some(n) => BufferSize::Fixed(n),
        None => BufferSize::Default,
    };

    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(sample_rate),
        buffer_size,
    };

    let capacity = (sample_rate as usize * channels as usize).max(65_536);
    let shared: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
    let shared_in = Arc::clone(&shared);
    let shared_out = Arc::clone(&shared);
    let max_fill = capacity;

    let in_stream = input_device
        .build_input_stream(
            &config,
            move |data: &[f32], _info| {
                if let Ok(mut buf) = shared_in.try_lock() {
                    for &s in data {
                        if buf.len() < max_fill {
                            buf.push_back(s);
                        }
                    }
                }
            },
            |err| eprintln!("[audio] input error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build input stream: {e}"))?;

    let out_stream = output_device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                if let Ok(mut buf) = shared_out.try_lock() {
                    for s in data.iter_mut() {
                        *s = buf.pop_front().unwrap_or(0.0);
                    }
                } else {
                    data.fill(0.0);
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {e}"))?;

    let info = RunningInfo {
        input_device: input_name,
        output_device: output_name,
        sample_rate,
        buffer_size: buffer_opt.0.to_string(),
        channels,
    };

    Ok((in_stream, out_stream, info))
}
