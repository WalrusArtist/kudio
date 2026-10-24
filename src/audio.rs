//! Audio device setup (cpal). On Linux this goes through ALSA, which is
//! routed through PipeWire/PulseAudio on most desktops.

use crate::engine::{Engine, EngineLink};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedBufferSize,
};
use rtrb::{Producer, RingBuffer};
use std::sync::mpsc::Sender;

pub struct AudioOptions {
    pub output: Option<String>,
    pub input: Option<String>,
    pub buffer: u32,
    pub mic: bool,
}

pub struct Audio {
    pub sample_rate: u32,
    pub out_name: String,
    pub in_name: Option<String>,
    pub buffer: Option<u32>,
    _out: Stream,
    _in: Option<Stream>,
}

const PREFERRED_RATE: u32 = 48000;

fn find_device(devices: impl Iterator<Item = Device>, needle: &str) -> Option<Device> {
    let needle = needle.to_lowercase();
    devices
        .into_iter()
        .find(|d| d.to_string().to_lowercase().contains(&needle))
}

pub fn list_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devs) = host.output_devices() {
        out.extend(devs.map(|d| format!("output: {d}")));
    }
    if let Ok(devs) = host.input_devices() {
        out.extend(devs.map(|d| format!("input:  {d}")));
    }
    out
}

/// Pick a stream config: f32 if possible, at `rate` if possible.
fn pick_config(
    configs: Vec<cpal::SupportedStreamConfigRange>,
    rate: u32,
    want_channels: Option<u16>,
) -> Option<cpal::SupportedStreamConfig> {
    let score = |c: &cpal::SupportedStreamConfigRange| {
        let fmt = match c.sample_format() {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::I32 => 2,
            _ => 9,
        };
        let ch = match want_channels {
            Some(w) if c.channels() == w => 0,
            Some(_) => 1,
            None => 0,
        };
        (fmt, ch)
    };
    let mut ok: Vec<_> = configs
        .into_iter()
        .filter(|c| c.min_sample_rate() <= rate && rate <= c.max_sample_rate())
        .filter(|c| {
            matches!(
                c.sample_format(),
                SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32
            )
        })
        .collect();
    ok.sort_by_key(score);
    ok.into_iter().next().map(|c| c.with_sample_rate(rate))
}

fn buffer_size(supported: &SupportedBufferSize, want: u32) -> BufferSize {
    match supported {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(want.clamp(*min, *max)),
        SupportedBufferSize::Unknown => BufferSize::Fixed(want),
    }
}

pub fn start(opts: &AudioOptions, errors: Sender<String>) -> Result<(Audio, EngineLink), String> {
    let host = cpal::default_host();

    let out_dev = match &opts.output {
        Some(n) => find_device(host.output_devices().map_err(|e| e.to_string())?, n)
            .ok_or_else(|| format!("no output device matching '{n}' (try --list-devices)"))?,
        None => host
            .default_output_device()
            .ok_or("no default audio output device")?,
    };
    let default_out = out_dev.default_output_config().map_err(|e| e.to_string())?;
    let out_supported = out_dev
        .supported_output_configs()
        .map(|c| c.collect::<Vec<_>>())
        .unwrap_or_default();
    let out_cfg = pick_config(out_supported.clone(), PREFERRED_RATE, Some(2))
        .or_else(|| pick_config(out_supported, default_out.sample_rate(), Some(2)))
        .unwrap_or(default_out);
    let sr = out_cfg.sample_rate();

    // Microphone: must run at the same rate as the output.
    let mut mic_note = None;
    let in_dev_cfg = if opts.mic {
        let dev = match &opts.input {
            Some(n) => host.input_devices().ok().and_then(|d| find_device(d, n)),
            None => host.default_input_device(),
        };
        match dev {
            Some(dev) => {
                let cfgs = dev
                    .supported_input_configs()
                    .map(|c| c.collect())
                    .unwrap_or_default();
                match pick_config(cfgs, sr, None) {
                    Some(cfg) => Some((dev, cfg)),
                    None => {
                        mic_note = Some(format!(
                            "microphone '{dev}' cannot run at {sr} Hz; voice module disabled"
                        ));
                        None
                    }
                }
            }
            None => {
                mic_note = Some("no microphone found; voice module disabled".to_string());
                None
            }
        }
    } else {
        None
    };
    if let Some(n) = mic_note {
        let _ = errors.send(n);
    }

    let (mic_tx, mic_rx) = if in_dev_cfg.is_some() {
        let (tx, rx) = RingBuffer::<f32>::new(sr as usize);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let (engine, link) = Engine::new(sr, mic_rx);
    let mut out_config: StreamConfig = out_cfg.config();
    out_config.buffer_size = buffer_size(out_cfg.buffer_size(), opts.buffer);
    let (out_stream, buffer) = match build_output(
        &out_dev,
        out_cfg.sample_format(),
        out_config,
        engine,
        errors.clone(),
    ) {
        Ok(s) => (s, Some(opts.buffer)),
        Err((e, engine)) => {
            // Some backends reject fixed buffer sizes; fall back to the default.
            out_config.buffer_size = BufferSize::Default;
            let s = build_output(
                &out_dev,
                out_cfg.sample_format(),
                out_config,
                engine,
                errors.clone(),
            )
            .map_err(|(e2, _)| format!("could not open audio output: {e}; {e2}"))?;
            (s, None)
        }
    };
    out_stream.play().map_err(|e| e.to_string())?;

    let mut in_name = None;
    let in_stream = match (in_dev_cfg, mic_tx) {
        (Some((dev, cfg)), Some(tx)) => {
            let mut config: StreamConfig = cfg.config();
            config.buffer_size = buffer_size(cfg.buffer_size(), opts.buffer);
            match build_input(&dev, cfg.sample_format(), config, tx, errors.clone()) {
                Ok(s) => match s.play() {
                    Ok(()) => {
                        in_name = Some(dev.to_string());
                        Some(s)
                    }
                    Err(e) => {
                        let _ = errors.send(format!("microphone failed to start: {e}"));
                        None
                    }
                },
                Err(e) => {
                    let _ = errors.send(format!("microphone unavailable: {e}"));
                    None
                }
            }
        }
        _ => None,
    };

    Ok((
        Audio {
            sample_rate: sr,
            out_name: out_dev.to_string(),
            in_name,
            buffer,
            _out: out_stream,
            _in: in_stream,
        },
        link,
    ))
}

#[allow(clippy::result_large_err)] // one-shot at startup; the engine is handed back for a retry
fn build_output(
    dev: &Device,
    fmt: SampleFormat,
    config: StreamConfig,
    engine: Engine,
    errors: Sender<String>,
) -> Result<Stream, (String, Engine)> {
    // The engine moves into the callback; on failure we get it back via a slot.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(engine)));
    let result = match fmt {
        SampleFormat::F32 => out_stream::<f32>(dev, config, slot.clone(), errors),
        SampleFormat::I16 => out_stream::<i16>(dev, config, slot.clone(), errors),
        SampleFormat::I32 => out_stream::<i32>(dev, config, slot.clone(), errors),
        other => Err(format!("unsupported output sample format {other}")),
    };
    match result {
        Ok(s) => Ok(s),
        Err(e) => {
            let engine = slot.lock().ok().and_then(|mut g| g.take());
            match engine {
                Some(engine) => Err((e, engine)),
                None => panic!("audio engine lost while opening the output: {e}"),
            }
        }
    }
}

fn out_stream<T>(
    dev: &Device,
    config: StreamConfig,
    slot: std::sync::Arc<std::sync::Mutex<Option<Engine>>>,
    errors: Sender<String>,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut engine: Option<Engine> = None;
    dev.build_output_stream(
        config,
        move |data: &mut [T], _| {
            // Take the engine out of the shared slot on the first callback so
            // the steady state is lock-free.
            if engine.is_none()
                && let Ok(mut g) = slot.try_lock()
            {
                engine = g.take();
            }
            match &mut engine {
                Some(e) => e.process(data, channels),
                None => data.fill(T::EQUILIBRIUM),
            }
        },
        move |e| {
            let _ = errors.send(format!("audio output: {e}"));
        },
        None,
    )
    .map_err(|e| e.to_string())
}

fn build_input(
    dev: &Device,
    fmt: SampleFormat,
    config: StreamConfig,
    tx: Producer<f32>,
    errors: Sender<String>,
) -> Result<Stream, String> {
    match fmt {
        SampleFormat::F32 => in_stream::<f32>(dev, config, tx, errors),
        SampleFormat::I16 => in_stream::<i16>(dev, config, tx, errors),
        SampleFormat::I32 => in_stream::<i32>(dev, config, tx, errors),
        other => Err(format!("unsupported input sample format {other}")),
    }
}

fn in_stream<T>(
    dev: &Device,
    config: StreamConfig,
    mut tx: Producer<f32>,
    errors: Sender<String>,
) -> Result<Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = (config.channels as usize).max(1);
    dev.build_input_stream(
        config,
        move |data: &[T], _| {
            // Downmix to mono and push; drop samples if the engine is not reading.
            let frames = data.len() / channels;
            let k = tx.slots().min(frames);
            if let Ok(chunk) = tx.write_chunk_uninit(k) {
                chunk.fill_from_iter(data.chunks_exact(channels).map(|f| {
                    f.iter().map(|s| f32::from_sample_(*s)).sum::<f32>() / channels as f32
                }));
            }
        },
        move |e| {
            let _ = errors.send(format!("audio input: {e}"));
        },
        None,
    )
    .map_err(|e| e.to_string())
}
