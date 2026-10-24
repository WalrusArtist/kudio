//! Session persistence, presets and session recording ("takes").
//!
//! Layout (default root `~/Music/kudio`, override with `$KUDIO_HOME`):
//!
//! ```text
//! sessions/<name>/session.json     parameters, loop length, track list
//! sessions/<name>/tracks/<id>.wav  voice loop audio (mono, 32-bit float)
//! sessions/<name>/takes/take-NNN.wav  recorded performances (stereo)
//! presets/<name>.json              a saved sound (params for one source)
//! ```

use crate::params::{self, Params, Scope};
use crate::track::{Kind, SeqEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

pub const VERSION: u32 = 1;

pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("KUDIO_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Music").join("kudio")
}

pub fn sessions_dir() -> PathBuf {
    root().join("sessions")
}

pub fn presets_dir() -> PathBuf {
    root().join("presets")
}

pub fn session_dir(name: &str) -> PathBuf {
    sessions_dir().join(name)
}

/// Only allow simple names so a session/preset name can never escape its folder.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with('.')
}

/// `session-YYYYMMDD-HHMMSS` (UTC), no date crate needed.
pub fn default_session_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "session-{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    pub sample_rate: u32,
    /// Master loop length in frames (set by the first recorded loop).
    pub loop_len: Option<u64>,
    /// Absolute start frame of the first loop (phase reference).
    #[serde(default)]
    pub loop_t0: u64,
    pub master: BTreeMap<String, f32>,
    pub live_synth: BTreeMap<String, f32>,
    pub live_voice: BTreeMap<String, f32>,
    /// Template for new drum tracks.
    #[serde(default)]
    pub live_drums: BTreeMap<String, f32>,
    /// Drum tempo. Fixed by the loop length once a loop exists.
    #[serde(default = "default_bpm")]
    pub bpm: f32,
    pub tracks: Vec<TrackFile>,
}

fn default_bpm() -> f32 {
    120.0
}

#[derive(Serialize, Deserialize)]
pub struct TrackFile {
    pub id: u32,
    pub name: String,
    pub kind: Kind,
    pub len: u64,
    pub t0: u64,
    #[serde(default)]
    pub muted: bool,
    pub params: BTreeMap<String, f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SeqEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    /// Drum tracks: how many bars the loop holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bars: Option<u32>,
}

/// UI-side description of a track (the audio side has its own copy).
#[derive(Clone)]
pub struct TrackModel {
    pub id: u32,
    pub name: String,
    pub kind: Kind,
    pub params: Params,
    pub len: u64,
    pub t0: u64,
    pub muted: bool,
    pub solo: bool,
    pub data: TrackData,
}

#[derive(Clone)]
pub enum TrackData {
    Synth(Arc<[SeqEvent]>),
    Voice(Arc<[f32]>),
    Drum { bars: u32 },
}

impl TrackModel {
    pub fn scope(&self) -> Scope {
        match self.kind {
            Kind::Synth => Scope::Synth { track: true },
            Kind::Voice => Scope::Voice { track: true },
            Kind::Drum => Scope::Drum { track: true },
        }
    }
}

pub struct Loaded {
    pub file: SessionFile,
    pub tracks: Vec<TrackModel>,
    pub master: Params,
    pub live_synth: Params,
    pub live_voice: Params,
    pub live_drums: Params,
    pub warnings: Vec<String>,
}

fn write_wav(path: &Path, data: &[f32], channels: u16, sr: u32) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w =
        hound::WavWriter::create(path, spec).map_err(|e| format!("{}: {e}", path.display()))?;
    for s in data {
        w.write_sample(*s).map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())
}

fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let mut r = hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect()
        }
    }
    .map_err(|e| e.to_string())?;
    let mono = raw
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    Ok((mono, spec.sample_rate))
}

fn resample(data: &[f32], ratio: f64) -> Vec<f32> {
    let n = (data.len() as f64 * ratio).round() as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 / ratio;
            let j = pos as usize;
            let frac = (pos - j as f64) as f32;
            let a = data.get(j).copied().unwrap_or(0.0);
            let b = data.get(j + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

pub struct SaveInput<'a> {
    pub sample_rate: u32,
    pub loop_len: Option<u64>,
    pub loop_t0: u64,
    pub master: &'a Params,
    pub live_synth: &'a Params,
    pub live_voice: &'a Params,
    pub live_drums: &'a Params,
    pub bpm: f32,
    pub tracks: &'a [TrackModel],
}

pub fn save(name: &str, s: &SaveInput) -> Result<PathBuf, String> {
    let dir = session_dir(name);
    let tdir = dir.join("tracks");
    fs::create_dir_all(&tdir).map_err(|e| format!("{}: {e}", tdir.display()))?;

    let mut tracks = Vec::new();
    let mut keep = Vec::new();
    for t in s.tracks {
        let mut tf = TrackFile {
            id: t.id,
            name: t.name.clone(),
            kind: t.kind,
            len: t.len,
            t0: t.t0 % t.len.max(1),
            muted: t.muted,
            params: params::to_map(&t.params, t.scope()),
            events: Vec::new(),
            audio: None,
            bars: None,
        };
        match &t.data {
            TrackData::Synth(ev) => tf.events = ev.to_vec(),
            TrackData::Drum { bars } => tf.bars = Some(*bars),
            TrackData::Voice(audio) => {
                let file = format!("{:03}.wav", t.id);
                let path = tdir.join(&file);
                // Voice audio never changes after recording, so write it once.
                if !path.exists() {
                    write_wav(&path, audio, 1, s.sample_rate)?;
                }
                keep.push(file.clone());
                tf.audio = Some(format!("tracks/{file}"));
            }
        }
        tracks.push(tf);
    }
    // Remove audio of tracks that were deleted.
    if let Ok(rd) = fs::read_dir(&tdir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".wav") && !keep.contains(&n) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    let file = SessionFile {
        version: VERSION,
        sample_rate: s.sample_rate,
        loop_len: s.loop_len,
        loop_t0: s.loop_len.map(|l| s.loop_t0 % l.max(1)).unwrap_or(0),
        master: params::to_map(s.master, Scope::Master),
        live_synth: params::to_map(s.live_synth, Scope::Synth { track: false }),
        live_voice: params::to_map(s.live_voice, Scope::Voice { track: false }),
        live_drums: params::to_map(s.live_drums, Scope::Drum { track: false }),
        bpm: s.bpm,
        tracks,
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    let path = dir.join("session.json");
    let tmp = dir.join("session.json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

pub fn exists(name: &str) -> bool {
    session_dir(name).join("session.json").exists()
}

pub fn load(name: &str, sr: u32) -> Result<Loaded, String> {
    let dir = session_dir(name);
    let path = dir.join("session.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: SessionFile =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut warnings = Vec::new();

    let ratio = sr as f64 / file.sample_rate.max(1) as f64;
    let rescale = |v: u64| (v as f64 * ratio).round() as u64;
    if file.sample_rate != sr {
        warnings.push(format!(
            "session was recorded at {} Hz, device runs at {sr} Hz: loops are resampled",
            file.sample_rate
        ));
    }

    let mut load_params = |map: &BTreeMap<String, f32>, what: &str| {
        let mut p = params::defaults();
        for k in params::apply_map(&mut p, map) {
            warnings.push(format!("{what}: unknown parameter '{k}' ignored"));
        }
        p
    };
    let master = load_params(&file.master, "master");
    let live_synth = load_params(&file.live_synth, "live synth");
    let live_voice = load_params(&file.live_voice, "live voice");
    let live_drums = load_params(&file.live_drums, "drum defaults");

    let loop_len = file.loop_len.map(rescale);
    let mut tracks = Vec::new();
    for tf in &file.tracks {
        let mut p = params::defaults();
        params::apply_map(&mut p, &tf.params);
        let len = rescale(tf.len).max(1);
        // Keep loops an exact multiple of the master loop after rescaling.
        let len = match (loop_len, file.loop_len) {
            (Some(l), Some(ol)) if ol > 0 => l * (tf.len / ol).max(1),
            _ => len,
        };
        let data = match tf.kind {
            Kind::Synth => {
                let mut ev: Vec<SeqEvent> = tf
                    .events
                    .iter()
                    .map(|e| SeqEvent {
                        at: rescale(e.at) % len,
                        ev: e.ev,
                    })
                    .collect();
                ev.sort_by_key(SeqEvent::order_key);
                TrackData::Synth(ev.into())
            }
            Kind::Drum => TrackData::Drum {
                bars: tf.bars.unwrap_or(1).max(1),
            },
            Kind::Voice => {
                let Some(rel) = &tf.audio else {
                    warnings.push(format!("track '{}' has no audio file, skipped", tf.name));
                    continue;
                };
                let (mut audio, wav_sr) = match read_wav_mono(&dir.join(rel)) {
                    Ok(a) => a,
                    Err(e) => {
                        warnings.push(format!("track '{}' skipped: {e}", tf.name));
                        continue;
                    }
                };
                if wav_sr != sr {
                    audio = resample(&audio, sr as f64 / wav_sr as f64);
                }
                audio.resize(len as usize, 0.0);
                TrackData::Voice(audio.into())
            }
        };
        tracks.push(TrackModel {
            id: tf.id,
            name: tf.name.clone(),
            kind: tf.kind,
            params: p,
            len,
            t0: rescale(tf.t0),
            muted: tf.muted,
            solo: false,
            data,
        });
    }
    Ok(Loaded {
        file,
        tracks,
        master,
        live_synth,
        live_voice,
        live_drums,
        warnings,
    })
}

pub fn list_sessions() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(sessions_dir())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().join("session.json").exists())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// --- presets -----------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct PresetFile {
    pub kind: String,
    pub params: BTreeMap<String, f32>,
}

pub fn save_preset(
    name: &str,
    kind: &str,
    params: BTreeMap<String, f32>,
) -> Result<PathBuf, String> {
    let dir = presets_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{name}.json"));
    let json = serde_json::to_string_pretty(&PresetFile {
        kind: kind.to_string(),
        params,
    })
    .map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

pub fn load_preset(name: &str) -> Result<PresetFile, String> {
    let path = presets_dir().join(format!("{name}.json"));
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn list_presets() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(presets_dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .strip_suffix(".json")
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// --- takes (recording the whole performance) ----------------------------------

pub struct TakeWriter {
    pub path: PathBuf,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl TakeWriter {
    /// Start a writer thread draining `rx` into a new numbered WAV file.
    pub fn start(
        session: &str,
        sr: u32,
        mut rx: rtrb::Consumer<f32>,
        log: Sender<String>,
    ) -> Result<Self, String> {
        let dir = session_dir(session).join("takes");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = (1..)
            .map(|i| dir.join(format!("take-{i:03}.wav")))
            .find(|p| !p.exists())
            .expect("unbounded range");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let shown = path.display().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("kudio-take".into())
            .spawn(move || {
                let mut err = None;
                loop {
                    let n = rx.slots();
                    if n > 0 {
                        if let Ok(chunk) = rx.read_chunk(n) {
                            let (a, b) = chunk.as_slices();
                            for s in a.iter().chain(b) {
                                if let Err(e) = w.write_sample(*s) {
                                    err.get_or_insert(e.to_string());
                                }
                            }
                            chunk.commit_all();
                        }
                    } else if rx.is_abandoned() || stop_flag.load(Ordering::Acquire) {
                        break;
                    } else {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                let dur = w.duration() as f32 / sr as f32;
                match w.finalize().err().map(|e| e.to_string()).or(err) {
                    Some(e) => {
                        let _ = log.send(format!("take {shown} failed: {e}"));
                    }
                    None => {
                        let _ = log.send(format!("take saved: {shown} ({dur:.1} s)"));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path,
            handle: Some(handle),
            stop,
        })
    }

    /// Drain what is left, finalize the file and wait (at most ~3 s) for the
    /// writer thread. Call after the audio stream is gone. The stop flag covers
    /// audio backends that don't drop the callback (and thus the producer)
    /// together with the stream.
    pub fn join(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert!(valid_name("my-jam_2"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name(""));
        assert!(!valid_name(".hidden"));
        assert!(default_session_name().starts_with("session-"));
        assert_eq!(
            default_session_name().len(),
            "session-20260101-000000".len()
        );
    }

    #[test]
    fn resample_doubles_length() {
        let out = resample(&[0.0, 1.0, 0.0, -1.0], 2.0);
        assert_eq!(out.len(), 8);
        assert!((out[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("kudio-test-{}", std::process::id()));
        // SAFETY: tests in this module are the only users of KUDIO_HOME.
        unsafe { std::env::set_var("KUDIO_HOME", &dir) };
        let mut synth = params::defaults();
        synth[params::p::CUTOFF] = 1234.0;
        let mut drum = params::defaults();
        drum[params::p::DRUM] = 1.0;
        drum[params::p::OFFSET] = 4.0;
        let tracks = vec![
            TrackModel {
                id: 1,
                name: "bass".into(),
                kind: Kind::Synth,
                params: synth,
                len: 1000,
                t0: 2500,
                muted: true,
                solo: false,
                data: TrackData::Synth(
                    vec![SeqEvent {
                        at: 10,
                        ev: crate::track::Ev::On { note: 40, vel: 99 },
                    }]
                    .into(),
                ),
            },
            TrackModel {
                id: 2,
                name: "vox".into(),
                kind: Kind::Voice,
                params: params::defaults(),
                len: 1000,
                t0: 0,
                muted: false,
                solo: false,
                data: TrackData::Voice(vec![0.25; 1000].into()),
            },
            TrackModel {
                id: 3,
                name: "snare-3".into(),
                kind: Kind::Drum,
                params: drum,
                len: 2000,
                t0: 0,
                muted: false,
                solo: false,
                data: TrackData::Drum { bars: 2 },
            },
        ];
        let d = params::defaults();
        save(
            "t",
            &SaveInput {
                sample_rate: 48000,
                loop_len: Some(1000),
                loop_t0: 0,
                master: &d,
                live_synth: &synth,
                live_voice: &d,
                live_drums: &d,
                bpm: 96.0,
                tracks: &tracks,
            },
        )
        .unwrap();
        let l = load("t", 48000).unwrap();
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.tracks.len(), 3);
        assert_eq!(l.file.bpm, 96.0);
        assert_eq!(l.tracks[0].params[params::p::CUTOFF], 1234.0);
        assert!(l.tracks[0].muted);
        assert_eq!(l.tracks[0].t0, 500);
        match &l.tracks[1].data {
            TrackData::Voice(a) => assert_eq!(a[3], 0.25),
            _ => panic!("expected voice"),
        }
        let snare = &l.tracks[2];
        assert!(matches!(snare.data, TrackData::Drum { bars: 2 }));
        assert_eq!(snare.len, 2000);
        assert_eq!(snare.params[params::p::DRUM], 1.0);
        assert_eq!(snare.params[params::p::OFFSET], 4.0);
        let _ = fs::remove_dir_all(&dir);
    }
}
