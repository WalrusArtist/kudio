//! The parameter table.
//!
//! Every tweakable value in kudio (synth patch, voice settings, per-track FX,
//! master bus) is a plain `f32` stored in a fixed-size [`Params`] array. The
//! [`SPECS`] table describes each slot: its name (used by terminal commands
//! and session files), range, default, step behaviour and display format.
//!
//! A single flat table keeps the audio thread simple (no maps, no strings)
//! while the UI and the session files refer to parameters by name.

use std::collections::BTreeMap;

/// Which kind of sound source a parameter applies to.
pub const SYN: u8 = 1;
pub const VOX: u8 = 2;
pub const MST: u8 = 4;
/// Only meaningful on the live input (not on a recorded track).
pub const LIVE: u8 = 8;
/// Only meaningful on a recorded track.
pub const TRK: u8 = 16;
pub const DRM: u8 = 32;
const FX: u8 = SYN | VOX | DRM;

#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// Add/subtract a fixed amount.
    Lin(f32),
    /// Multiply/divide by a ratio (frequencies, times).
    Exp(f32),
}

#[derive(Clone, Copy, Debug)]
pub enum Fmt {
    Num(usize),
    Hz,
    Sec,
    Ms,
    Pct,
    Semis,
    Cents,
    Oct,
    Choice(&'static [&'static str]),
    Toggle,
    /// A multiplier, shown as `×1.50`.
    Times,
}

pub struct Spec {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub step: Step,
    pub fmt: Fmt,
    pub flags: u8,
    pub help: &'static str,
}

pub const WAVES: &[&str] = &["sine", "tri", "saw", "square"];
pub const DRUMS: &[&str] = &["kick", "snare", "hihat", "cymbal"];
/// Drum hit rates; the number of hits per bar is `RATE_DIVS[rate]`.
pub const RATES: &[&str] = &["1/1", "1/2", "1/4", "1/8", "1/16"];
pub const RATE_DIVS: [u32; 5] = [1, 2, 4, 8, 16];

macro_rules! params {
    ($( $id:ident = $name:literal, $min:expr, $max:expr, $def:expr, $step:expr, $fmt:expr, $flags:expr, $help:literal; )*) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms, dead_code)]
        #[repr(usize)]
        enum Ix { $($id),* }
        /// Parameter indices into a [`Params`] array.
        #[allow(dead_code)]
        pub mod p { $( pub const $id: usize = super::Ix::$id as usize; )* }
        pub const SPECS: &[Spec] = &[ $( Spec {
            name: $name, min: $min, max: $max, default: $def,
            step: $step, fmt: $fmt, flags: $flags, help: $help,
        } ),* ];
    };
}

use Fmt::*;
use Step::*;

params! {
    // --- synth: oscillators -------------------------------------------------
    WAVE      = "wave",     0.0, 3.0, 2.0,      Lin(1.0),  Choice(WAVES), SYN, "oscillator 1 waveform (sine/tri/saw/square)";
    WAVE2     = "wave2",    0.0, 3.0, 2.0,      Lin(1.0),  Choice(WAVES), SYN, "oscillator 2 waveform";
    SEMI2     = "semi2",  -24.0, 24.0, 0.0,     Lin(1.0),  Semis,         SYN, "oscillator 2 interval in semitones";
    DETUNE    = "detune",   0.0, 50.0, 8.0,     Lin(1.0),  Cents,         SYN, "oscillator 2 detune in cents";
    MIX       = "mix",      0.0, 1.0, 0.5,      Lin(0.05), Pct,           SYN, "oscillator 2 level";
    SUB       = "sub",      0.0, 1.0, 0.0,      Lin(0.05), Pct,           SYN, "sub oscillator (sine, -1 octave) level";
    NOISE     = "noise",    0.0, 1.0, 0.0,      Lin(0.05), Pct,           SYN, "white noise level";
    OCTAVE    = "octave",  -3.0, 3.0, 0.0,      Lin(1.0),  Num(0),        SYN, "transpose in octaves";
    GLIDE     = "glide",    0.0, 1.0, 0.0,      Lin(0.02), Sec,           SYN, "portamento time";
    VEL       = "vel",      0.0, 1.0, 0.7,      Lin(0.05), Pct,           SYN, "velocity sensitivity";
    // --- synth: envelope / filter / lfo ------------------------------------
    ATTACK    = "attack",   0.001, 5.0, 0.005,  Exp(1.25), Sec,           SYN, "amp/filter envelope attack";
    DECAY     = "decay",    0.005, 5.0, 0.3,    Exp(1.25), Sec,           SYN, "envelope decay";
    SUSTAIN   = "sustain",  0.0, 1.0, 0.7,      Lin(0.05), Pct,           SYN, "envelope sustain level";
    RELEASE   = "release",  0.005, 8.0, 0.25,   Exp(1.25), Sec,           SYN, "envelope release";
    CUTOFF    = "cutoff",   40.0, 18000.0, 3000.0, Exp(1.122), Hz,        SYN, "low-pass filter cutoff";
    RESO      = "reso",     0.0, 1.0, 0.2,      Lin(0.05), Pct,           SYN, "filter resonance";
    FENV      = "fenv",    -4.0, 6.0, 1.5,      Lin(0.25), Oct,           SYN, "envelope -> cutoff amount (octaves)";
    LFORATE   = "lforate",  0.05, 20.0, 4.0,    Exp(1.15), Hz,            SYN, "LFO speed";
    VIBRATO   = "vibrato",  0.0, 2.0, 0.0,      Lin(0.05), Semis,         SYN, "LFO -> pitch depth (semitones)";
    WOBBLE    = "wobble",   0.0, 4.0, 0.0,      Lin(0.1),  Oct,           SYN, "LFO -> cutoff depth (octaves)";
    // --- FX chain (synth + voice, live + tracks) ----------------------------
    LEVEL     = "level",    0.0, 1.5, 0.8,      Lin(0.05), Pct,           FX,  "output level";
    PAN       = "pan",     -1.0, 1.0, 0.0,      Lin(0.1),  Num(2),        FX,  "stereo position (-1 left, +1 right)";
    DRIVE     = "drive",    0.0, 1.0, 0.0,      Lin(0.05), Pct,           FX,  "saturation";
    CRUSH     = "crush",    0.0, 1.0, 0.0,      Lin(0.05), Pct,           FX,  "bit/sample-rate crusher";
    HP        = "hp",       20.0, 2000.0, 20.0, Exp(1.122), Hz,           FX,  "high-pass filter (20 = off)";
    LP        = "lp",       200.0, 20000.0, 20000.0, Exp(1.122), Hz,      FX,  "low-pass filter (20000 = off)";
    DTIME     = "dtime",    10.0, 1000.0, 375.0, Exp(1.06), Ms,           FX,  "delay time";
    DFB       = "dfb",      0.0, 0.95, 0.35,    Lin(0.05), Pct,           FX,  "delay feedback";
    DMIX      = "dmix",     0.0, 1.0, 0.0,      Lin(0.05), Pct,           FX,  "delay amount";
    REVERB    = "reverb",   0.0, 1.0, 0.15,     Lin(0.05), Pct,           FX,  "send to the master reverb";
    // --- voice ---------------------------------------------------------------
    PITCH     = "pitch",  -24.0, 24.0, 0.0,     Lin(1.0),  Semis,         VOX, "pitch shift in semitones";
    REVERSE   = "reverse",  0.0, 1.0, 0.0,      Lin(1.0),  Toggle,        VOX | TRK,  "play the loop backwards";
    MONITOR   = "monitor",  0.0, 1.0, 0.0,      Lin(1.0),  Toggle,        VOX | LIVE, "hear the mic live (use headphones!)";
    INGAIN    = "ingain",   0.0, 4.0, 1.0,      Lin(0.1),  Pct,           VOX | LIVE, "microphone input gain";
    // --- drums -----------------------------------------------------------------
    DRUM      = "drum",     0.0, 3.0, 0.0,      Lin(1.0),  Choice(DRUMS), DRM | TRK,  "instrument (kick/snare/hihat/cymbal)";
    TUNE      = "tune",   -24.0, 24.0, 0.0,     Lin(1.0),  Semis,         DRM, "drum pitch in semitones";
    DDECAY    = "ddecay",   0.1, 4.0, 1.0,      Exp(1.15), Times,         DRM, "drum decay length (multiplier)";
    TONE      = "tone",     0.0, 1.0, 0.5,      Lin(0.05), Pct,           DRM, "kick click / snare noise / cymbal brightness";
    PUNCH     = "punch",    0.0, 1.0, 0.5,      Lin(0.05), Pct,           DRM, "attack: pitch drop and transient";
    RATE      = "rate",     0.0, 4.0, 2.0,      Lin(1.0),  Choice(RATES), DRM | TRK,  "how often it hits (1/4 = every beat)";
    OFFSET    = "offset",   0.0, 15.0, 0.0,     Lin(1.0),  Num(0),        DRM | TRK,  "shift the hits later, in 16th notes";
    // --- master ---------------------------------------------------------------
    VOLUME    = "volume",   0.0, 1.5, 0.8,      Lin(0.05), Pct,           MST, "master volume";
    ROOMSIZE  = "roomsize", 0.0, 1.0, 0.7,      Lin(0.05), Pct,           MST, "reverb room size";
    DAMP      = "damp",     0.0, 1.0, 0.4,      Lin(0.05), Pct,           MST, "reverb high-frequency damping";
}

pub const NUM: usize = SPECS.len();
pub type Params = [f32; NUM];

pub fn defaults() -> Params {
    let mut out = [0.0; NUM];
    for (v, s) in out.iter_mut().zip(SPECS) {
        *v = s.default;
    }
    out
}

/// What a parameter set belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Synth { track: bool },
    Voice { track: bool },
    Drum { track: bool },
    Master,
}

impl Scope {
    pub fn applies(self, idx: usize) -> bool {
        let f = SPECS[idx].flags;
        let (kind, track) = match self {
            Scope::Synth { track } => (SYN, track),
            Scope::Voice { track } => (VOX, track),
            Scope::Drum { track } => (DRM, track),
            Scope::Master => return f & MST != 0,
        };
        if f & kind == 0 {
            return false;
        }
        if track && f & LIVE != 0 {
            return false;
        }
        if !track && f & TRK != 0 {
            return false;
        }
        true
    }

    pub fn pages(self) -> Vec<Page> {
        let pages: &[Pd] = match self {
            Scope::Synth { .. } => SYNTH_PAGES,
            Scope::Voice { .. } => VOICE_PAGES,
            Scope::Drum { .. } => DRUM_PAGES,
            Scope::Master => MASTER_PAGES,
        };
        pages
            .iter()
            .map(|pg| Page {
                title: pg.title,
                params: pg
                    .params
                    .iter()
                    .copied()
                    .filter(|&i| self.applies(i))
                    .collect(),
            })
            .collect()
    }
}

/// A page of up to 10 parameters, one per control-mode key column.
#[derive(Clone)]
pub struct Page {
    pub title: &'static str,
    pub params: Vec<usize>,
}

struct Pd {
    title: &'static str,
    params: &'static [usize],
}

const FX_PAGE: &[usize] = &[
    p::LEVEL,
    p::PAN,
    p::DRIVE,
    p::CRUSH,
    p::HP,
    p::LP,
    p::DTIME,
    p::DFB,
    p::DMIX,
    p::REVERB,
];

const SYNTH_PAGES: &[Pd] = &[
    Pd {
        title: "osc",
        params: &[
            p::WAVE,
            p::WAVE2,
            p::SEMI2,
            p::DETUNE,
            p::MIX,
            p::SUB,
            p::NOISE,
            p::OCTAVE,
            p::GLIDE,
            p::VEL,
        ],
    },
    Pd {
        title: "env/filter/lfo",
        params: &[
            p::ATTACK,
            p::DECAY,
            p::SUSTAIN,
            p::RELEASE,
            p::CUTOFF,
            p::RESO,
            p::FENV,
            p::LFORATE,
            p::VIBRATO,
            p::WOBBLE,
        ],
    },
    Pd {
        title: "fx",
        params: FX_PAGE,
    },
];

const VOICE_PAGES: &[Pd] = &[
    Pd {
        title: "fx",
        params: FX_PAGE,
    },
    Pd {
        title: "voice",
        params: &[p::PITCH, p::REVERSE, p::MONITOR, p::INGAIN],
    },
];

const DRUM_PAGES: &[Pd] = &[
    Pd {
        title: "drum",
        params: &[
            p::DRUM,
            p::TUNE,
            p::DDECAY,
            p::TONE,
            p::PUNCH,
            p::RATE,
            p::OFFSET,
        ],
    },
    Pd {
        title: "fx",
        params: FX_PAGE,
    },
];

const MASTER_PAGES: &[Pd] = &[Pd {
    title: "master",
    params: &[p::VOLUME, p::ROOMSIZE, p::DAMP],
}];

pub fn find(name: &str) -> Option<usize> {
    let name = name.to_ascii_lowercase();
    SPECS.iter().position(|s| s.name == name)
}

pub fn clamp(idx: usize, v: f32) -> f32 {
    let s = &SPECS[idx];
    let v = v.clamp(s.min, s.max);
    match s.fmt {
        Choice(_) | Toggle => v.round(),
        _ => v,
    }
}

/// Move a value one step up (`dir > 0`) or down. `coarse` takes bigger steps.
pub fn nudge(idx: usize, v: f32, dir: i32, coarse: bool) -> f32 {
    let s = &SPECS[idx];
    let d = dir.signum() as f32;
    let next = match (s.step, s.fmt) {
        // Discrete params wrap around so a single key can cycle through them.
        (_, Choice(names)) => {
            let n = names.len() as f32;
            (v + d).rem_euclid(n)
        }
        (_, Toggle) => 1.0 - v.round(),
        (Lin(step), _) => v + d * step * if coarse { 5.0 } else { 1.0 },
        (Exp(ratio), _) => v * ratio.powf(d * if coarse { 6.0 } else { 1.0 }),
    };
    clamp(idx, next)
}

pub fn format(idx: usize, v: f32) -> String {
    match SPECS[idx].fmt {
        Num(d) => format!("{v:.d$}"),
        Hz if v >= 1000.0 => format!("{:.2} kHz", v / 1000.0),
        Hz => format!("{v:.1} Hz"),
        Sec if v < 1.0 => format!("{:.0} ms", v * 1000.0),
        Sec => format!("{v:.2} s"),
        Ms => format!("{v:.0} ms"),
        Pct => format!("{:.0}%", v * 100.0),
        Semis => {
            if v.fract().abs() < 1e-3 {
                format!("{v:+.0} st")
            } else {
                format!("{v:+.2} st")
            }
        }
        Cents => format!("{v:.0} ct"),
        Oct => format!("{v:+.2} oct"),
        Choice(names) => names.get(v as usize).copied().unwrap_or("?").to_string(),
        Toggle => if v >= 0.5 { "on" } else { "off" }.to_string(),
        Times => format!("×{v:.2}"),
    }
}

/// Parse a user-typed value: plain numbers, `2k`, `50%`, choice names, on/off.
pub fn parse_value(idx: usize, text: &str) -> Result<f32, String> {
    let s = &SPECS[idx];
    let t = text.trim().to_ascii_lowercase();
    if let Choice(names) = s.fmt
        && let Some(i) = names.iter().position(|n| *n == t)
    {
        return Ok(i as f32);
    }
    if let Toggle = s.fmt {
        match t.as_str() {
            "on" | "true" | "yes" => return Ok(1.0),
            "off" | "false" | "no" => return Ok(0.0),
            _ => {}
        }
    }
    let (num, mult) = if let Some(n) = t.strip_suffix('%') {
        (n, 0.01)
    } else if let Some(n) = t.strip_suffix("khz").or_else(|| t.strip_suffix('k')) {
        (n, 1000.0)
    } else if let Some(n) = t.strip_suffix("ms") {
        // Seconds-based params accept ms; ms-based params take the number as is.
        (n, if matches!(s.fmt, Sec) { 0.001 } else { 1.0 })
    } else {
        (
            t.trim_end_matches("hz")
                .trim_end_matches('s')
                .trim_end_matches(['x', '×']),
            1.0,
        )
    };
    let v: f32 = num
        .trim()
        .parse()
        .map_err(|_| format!("'{text}' is not a valid value for {}", s.name))?;
    Ok(clamp(idx, v * mult))
}

/// Serialize the params that apply to `scope` by name.
pub fn to_map(params: &Params, scope: Scope) -> BTreeMap<String, f32> {
    (0..NUM)
        .filter(|&i| scope.applies(i))
        .map(|i| (SPECS[i].name.to_string(), params[i]))
        .collect()
}

/// Apply named values onto `params`. Unknown names are returned so callers can warn.
pub fn apply_map(params: &mut Params, map: &BTreeMap<String, f32>) -> Vec<String> {
    let mut unknown = Vec::new();
    for (k, v) in map {
        match find(k) {
            Some(i) => params[i] = clamp(i, *v),
            None => unknown.push(k.clone()),
        }
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_match_names() {
        assert_eq!(SPECS[p::WAVE].name, "wave");
        assert_eq!(SPECS[p::CUTOFF].name, "cutoff");
        assert_eq!(SPECS[p::REVERB].name, "reverb");
        assert_eq!(SPECS[p::DAMP].name, "damp");
        assert_eq!(NUM, p::DAMP + 1);
    }

    #[test]
    fn defaults_are_in_range() {
        for s in SPECS {
            assert!(s.default >= s.min && s.default <= s.max, "{}", s.name);
        }
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(parse_value(p::WAVE, "square").unwrap(), 3.0);
        assert_eq!(parse_value(p::CUTOFF, "2k").unwrap(), 2000.0);
        assert_eq!(parse_value(p::ATTACK, "50ms").unwrap(), 0.05);
        assert_eq!(parse_value(p::MIX, "25%").unwrap(), 0.25);
        assert_eq!(parse_value(p::REVERSE, "on").unwrap(), 1.0);
        assert_eq!(format(p::WAVE, 0.0), "sine");
        assert!(parse_value(p::MIX, "abc").is_err());
    }

    #[test]
    fn scopes_filter_params() {
        let live_voice = Scope::Voice { track: false };
        let voice_track = Scope::Voice { track: true };
        assert!(live_voice.applies(p::MONITOR));
        assert!(!voice_track.applies(p::MONITOR));
        assert!(voice_track.applies(p::REVERSE));
        assert!(!live_voice.applies(p::REVERSE));
        assert!(!Scope::Synth { track: true }.applies(p::PITCH));
        assert!(Scope::Master.applies(p::VOLUME));
        assert_eq!(Scope::Synth { track: false }.pages().len(), 3);
        let live_drums = Scope::Drum { track: false };
        let drum_track = Scope::Drum { track: true };
        assert!(live_drums.applies(p::TUNE) && live_drums.applies(p::LEVEL));
        assert!(!live_drums.applies(p::RATE) && drum_track.applies(p::RATE));
        assert!(!drum_track.applies(p::CUTOFF));
        assert!(!Scope::Synth { track: true }.applies(p::PUNCH));
        assert_eq!(parse_value(p::DDECAY, "2x").unwrap(), 2.0);
        assert_eq!(parse_value(p::RATE, "1/8").unwrap(), 3.0);
    }

    #[test]
    fn nudge_wraps_choices() {
        assert_eq!(nudge(p::WAVE, 3.0, 1, false), 0.0);
        assert_eq!(nudge(p::WAVE, 0.0, -1, false), 3.0);
        assert!((nudge(p::MIX, 0.5, 1, false) - 0.55).abs() < 1e-6);
    }
}
