//! Drum machine: four synthesized instruments and the hit-pattern math used
//! by drum loop tracks. Nothing here allocates; hits are computed on the fly
//! from the track length, its number of bars and the `rate`/`offset` params.

use crate::dsp::Svf;
use crate::params::{DRUMS, Params, RATE_DIVS, p};
use std::f32::consts::TAU;

pub const KICK: u8 = 0;
pub const SNARE: u8 = 1;
pub const HIHAT: u8 = 2;
pub const CYMBAL: u8 = 3;

/// Default pattern per instrument as (`rate` index, `offset` in 16ths):
/// kick on every beat, snare on 2 and 4, hihat on the off-beats, cymbal once a bar.
pub const DEFAULT_PATTERN: [(f32, f32); 4] = [(2.0, 0.0), (1.0, 4.0), (2.0, 2.0), (0.0, 0.0)];

pub fn name(inst: u8) -> &'static str {
    DRUMS.get(inst as usize).copied().unwrap_or("drum")
}

/// Classic TR-808 metallic oscillator frequencies (Hz) for hihat and cymbal.
const METAL: [f32; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

/// Per-sample multiplier that decays by 60 dB in `secs`.
fn t60(secs: f32, sr: f32) -> f32 {
    (-6.9 / (secs.max(0.001) * sr)).exp()
}

/// One drum voice (retriggered on every hit, so it is monophonic).
pub struct DrumVoice {
    sr: f32,
    kind: u8,
    tune: f32,
    tone: f32,
    punch: f32,
    amp: f32,
    amp_k: f32,
    /// Secondary envelope: pitch sweep (kick/snare) or noise/transient (others).
    env2: f32,
    env2_k: f32,
    phase: f32,
    metal: [f32; 6],
    rng: u32,
    hp1: Svf,
    hp2: Svf,
}

impl DrumVoice {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            kind: KICK,
            tune: 1.0,
            tone: 0.5,
            punch: 0.5,
            amp: 0.0,
            amp_k: 0.0,
            env2: 0.0,
            env2_k: 0.0,
            phase: 0.0,
            metal: [0.0; 6],
            rng: 0x9E37_79B9,
            hp1: Svf::default(),
            hp2: Svf::default(),
        }
    }

    /// Read the parameters (once per audio block).
    pub fn prepare(&mut self, prm: &Params) {
        let sr = self.sr;
        let kind = (prm[p::DRUM] as u8).min(CYMBAL);
        if kind != self.kind {
            self.kind = kind;
            self.amp = 0.0;
            self.env2 = 0.0;
        }
        self.tune = (prm[p::TUNE] / 12.0).exp2();
        self.tone = prm[p::TONE];
        self.punch = prm[p::PUNCH];
        let d = prm[p::DDECAY];
        let (amp_s, env2_s, hp) = match kind {
            KICK => (0.45 * d, 0.03 + 0.04 * self.punch, 0.0),
            SNARE => (0.12 * d, 0.22 * d, 1200.0),
            HIHAT => (0.07 * d, 0.012, 6000.0 + 4000.0 * self.tone),
            _ => (1.3 * d, 0.02, 3500.0 + 4500.0 * self.tone),
        };
        self.amp_k = t60(amp_s, sr);
        self.env2_k = t60(env2_s, sr);
        if hp > 0.0 {
            let fc = hp * self.tune.sqrt();
            self.hp1.set(fc, 0.2, sr);
            self.hp2.set(fc, 0.2, sr);
        }
    }

    pub fn trigger(&mut self) {
        self.amp = 1.0;
        self.env2 = 1.0;
        self.phase = 0.0;
    }

    #[inline]
    fn noise(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng as f32 / u32::MAX as f32 * 2.0 - 1.0
    }

    #[inline]
    fn metal(&mut self) -> f32 {
        let mut s = 0.0;
        for (ph, f) in self.metal.iter_mut().zip(METAL) {
            *ph = (*ph + f * self.tune / self.sr).fract();
            s += if *ph < 0.5 { 1.0 } else { -1.0 };
        }
        s / 6.0
    }

    /// Render (overwrite) `out` with the dry drum signal.
    pub fn render(&mut self, out: &mut [f32]) {
        if self.amp < 1e-4 && self.env2 < 1e-4 {
            out.fill(0.0);
            return;
        }
        let sr = self.sr;
        for o in out.iter_mut() {
            let x = match self.kind {
                KICK => {
                    let f = 48.0 * self.tune * (1.0 + 5.0 * self.punch * self.env2);
                    self.phase = (self.phase + f / sr).fract();
                    let body = (self.phase * TAU).sin() * self.amp;
                    let click = self.noise() * self.env2 * self.env2 * self.tone * 0.5;
                    ((body + click) * (1.2 + self.punch)).tanh() * 0.9
                }
                SNARE => {
                    let f = 185.0 * self.tune * (1.0 + 0.6 * self.punch * self.env2 * self.env2);
                    self.phase = (self.phase + f / sr).fract();
                    let body = (self.phase * TAU).sin() * self.amp;
                    let n = self.noise();
                    let n = self.hp1.hp(n) * self.env2;
                    body * (1.0 - self.tone) * 0.8 + n * (0.25 + self.tone) * 0.75
                }
                kind => {
                    let m = self.metal();
                    let n = self.noise();
                    let x = self.hp2.hp(self.hp1.hp(0.65 * m + 0.35 * n));
                    let gain = if kind == HIHAT { 1.4 } else { 0.9 };
                    x * (self.amp + self.punch * self.env2) * gain
                }
            };
            self.amp *= self.amp_k;
            self.env2 *= self.env2_k;
            *o = x;
        }
    }
}

/// Where the hits of a drum track fall inside its loop.
#[derive(Clone, Copy, Debug)]
pub struct Pattern {
    first: f64,
    interval: f64,
}

impl Pattern {
    pub fn new(len: u64, bars: u32, prm: &Params) -> Self {
        let bar = len as f64 / bars.max(1) as f64;
        let rate = (prm[p::RATE].max(0.0) as usize).min(RATE_DIVS.len() - 1);
        let interval = (bar / RATE_DIVS[rate] as f64).max(1.0);
        let first = (prm[p::OFFSET].round().max(0.0) as f64 * bar / 16.0) % interval;
        Self { first, interval }
    }

    /// The first hit at or after `pos`. A result `>= len` means "no more hits
    /// in this pass of the loop".
    pub fn next_hit(&self, pos: u64) -> u64 {
        let mut k = ((pos as f64 - self.first) / self.interval).floor().max(0.0) as u64;
        loop {
            let h = (self.first + k as f64 * self.interval).round() as u64;
            if h >= pos {
                return h;
            }
            k += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::defaults;

    fn hits(len: u64, bars: u32, inst: u8) -> Vec<u64> {
        let mut prm = defaults();
        let (rate, offset) = DEFAULT_PATTERN[inst as usize];
        prm[p::RATE] = rate;
        prm[p::OFFSET] = offset;
        let pat = Pattern::new(len, bars, &prm);
        let mut v = Vec::new();
        let mut pos = 0;
        loop {
            let h = pat.next_hit(pos);
            if h >= len {
                return v;
            }
            v.push(h);
            pos = h + 1;
        }
    }

    #[test]
    fn default_patterns() {
        // One bar at 120 bpm and 48 kHz = 96000 frames, a beat = 24000.
        assert_eq!(hits(96000, 1, KICK), [0, 24000, 48000, 72000]);
        assert_eq!(hits(96000, 1, SNARE), [24000, 72000]);
        assert_eq!(hits(96000, 1, HIHAT), [12000, 36000, 60000, 84000]);
        assert_eq!(hits(96000, 1, CYMBAL), [0]);
        assert_eq!(hits(192000, 2, CYMBAL), [0, 96000]);
    }

    #[test]
    fn offset_wraps_inside_interval() {
        let mut prm = defaults();
        prm[p::RATE] = 2.0; // 1/4
        prm[p::OFFSET] = 5.0; // one beat + one 16th -> lands one 16th after each beat
        let pat = Pattern::new(96000, 1, &prm);
        assert_eq!(pat.next_hit(0), 6000);
        assert_eq!(pat.next_hit(6001), 30000);
    }

    #[test]
    fn every_drum_makes_bounded_sound() {
        for inst in KICK..=CYMBAL {
            let mut prm = defaults();
            prm[p::DRUM] = inst as f32;
            let mut v = DrumVoice::new(48000.0);
            v.prepare(&prm);
            v.trigger();
            let mut buf = vec![0.0; 4800];
            v.render(&mut buf);
            let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak > 0.05 && peak < 1.5, "{}: peak {peak}", name(inst));
        }
    }
}
