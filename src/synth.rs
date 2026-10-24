//! Polyphonic subtractive synthesizer.
//!
//! Each voice: osc1 + osc2 (+ sub sine + noise) -> resonant low-pass -> ADSR.
//! A single instance is used for live playing and every synth loop track owns
//! its own instance, so a loop can be re-patched after it was recorded.

use crate::dsp::{Adsr, Svf, osc};
use crate::params::{Params, p};
use std::f32::consts::TAU;

pub const MAX_VOICES: usize = 8;
/// Filter / pitch modulation is recomputed every N samples.
const CONTROL_RATE: u32 = 16;

#[derive(Clone, Copy, Default)]
struct Patch {
    w1: u8,
    w2: u8,
    ratio2: f32,
    mix: f32,
    sub: f32,
    noise: f32,
    transpose: f32,
    glide_coef: f32,
    vel_sens: f32,
    cutoff: f32,
    reso: f32,
    fenv: f32,
    lfo_inc: f32,
    vibrato: f32,
    wobble: f32,
}

#[derive(Clone, Copy, Default)]
struct Voice {
    active: bool,
    held: bool,
    sustained: bool,
    note: u8,
    target: f32,
    cur: f32,
    gain: f32,
    ph1: f32,
    ph2: f32,
    phs: f32,
    dt1: f32,
    dt2: f32,
    env: Adsr,
    filt: Svf,
    age: u64,
    ctl: u32,
}

pub struct Synth {
    sr: f32,
    voices: [Voice; MAX_VOICES],
    patch: Patch,
    sustain: bool,
    bend: f32,
    modwheel: f32,
    lfo_phase: f32,
    age: u64,
    last_note: Option<f32>,
    rng: u32,
}

impl Synth {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            voices: [Voice::default(); MAX_VOICES],
            patch: Patch::default(),
            sustain: false,
            bend: 0.0,
            modwheel: 0.0,
            lfo_phase: 0.0,
            age: 0,
            last_note: None,
            rng: 0x1234_5678,
        }
    }

    /// Pull the latest parameter values in (called once per audio block).
    pub fn prepare(&mut self, prm: &Params) {
        let sr = self.sr;
        let glide = prm[p::GLIDE];
        self.patch = Patch {
            w1: prm[p::WAVE] as u8,
            w2: prm[p::WAVE2] as u8,
            ratio2: ((prm[p::SEMI2] + prm[p::DETUNE] * 0.01) / 12.0).exp2(),
            mix: prm[p::MIX],
            sub: prm[p::SUB],
            noise: prm[p::NOISE],
            transpose: prm[p::OCTAVE] * 12.0,
            glide_coef: if glide < 0.001 {
                1.0
            } else {
                1.0 - (-(CONTROL_RATE as f32) / (glide * 0.25 * sr)).exp()
            },
            vel_sens: prm[p::VEL],
            cutoff: prm[p::CUTOFF],
            reso: prm[p::RESO],
            fenv: prm[p::FENV],
            lfo_inc: prm[p::LFORATE] / sr,
            vibrato: prm[p::VIBRATO],
            wobble: prm[p::WOBBLE],
        };
        for v in &mut self.voices {
            v.env.set(
                prm[p::ATTACK],
                prm[p::DECAY],
                prm[p::SUSTAIN],
                prm[p::RELEASE],
                sr,
            );
        }
    }

    pub fn note_on(&mut self, note: u8, vel: u8) {
        self.age += 1;
        let idx = self
            .voices
            .iter()
            .position(|v| v.active && v.note == note)
            .or_else(|| self.voices.iter().position(|v| !v.active))
            .unwrap_or_else(|| {
                // Steal: prefer the oldest released voice, otherwise the oldest.
                let mut best = 0;
                let mut best_key = (true, u64::MAX);
                for (i, v) in self.voices.iter().enumerate() {
                    let key = (v.held, v.age);
                    if key < best_key {
                        best_key = key;
                        best = i;
                    }
                }
                best
            });
        let target = note as f32;
        let v = &mut self.voices[idx];
        let was_active = v.active;
        v.active = true;
        v.held = true;
        v.sustained = false;
        v.note = note;
        v.target = target;
        v.cur = match self.last_note {
            Some(last) if self.patch.glide_coef < 1.0 => last,
            _ => target,
        };
        let vn = vel as f32 / 127.0;
        v.gain = 1.0 - self.patch.vel_sens + self.patch.vel_sens * vn * vn;
        v.age = self.age;
        v.ctl = 0;
        if !was_active {
            v.filt.reset();
            v.ph1 = 0.0;
            v.ph2 = 0.25;
            v.phs = 0.0;
        }
        v.env.gate_on();
        self.last_note = Some(target);
    }

    pub fn note_off(&mut self, note: u8) {
        for v in self
            .voices
            .iter_mut()
            .filter(|v| v.active && v.held && v.note == note)
        {
            v.held = false;
            if self.sustain {
                v.sustained = true;
            } else {
                v.env.gate_off();
            }
        }
    }

    pub fn set_sustain(&mut self, down: bool) {
        self.sustain = down;
        if !down {
            for v in self.voices.iter_mut().filter(|v| v.sustained) {
                v.sustained = false;
                v.env.gate_off();
            }
        }
    }

    pub fn set_bend(&mut self, semis: f32) {
        self.bend = semis;
    }

    pub fn set_mod(&mut self, amount: f32) {
        self.modwheel = amount;
    }

    /// Release every note (keeps release tails).
    pub fn all_off(&mut self) {
        self.sustain = false;
        for v in &mut self.voices {
            v.held = false;
            v.sustained = false;
            v.env.gate_off();
        }
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// Render a mono block (overwrites `out`).
    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        let pt = self.patch;
        let sr = self.sr;
        let n = out.len();
        let vib = pt.vibrato + self.modwheel * 0.5;
        let lfo0 = self.lfo_phase;
        for v in self.voices.iter_mut().filter(|v| v.active) {
            for (i, o) in out.iter_mut().enumerate() {
                if v.ctl % CONTROL_RATE == 0 {
                    let lfo = ((lfo0 + pt.lfo_inc * i as f32) * TAU).sin();
                    v.cur += (v.target - v.cur) * pt.glide_coef;
                    let pitch = v.cur + pt.transpose + self.bend + vib * lfo;
                    let freq = 440.0 * ((pitch - 69.0) / 12.0).exp2();
                    v.dt1 = (freq / sr).min(0.49);
                    v.dt2 = (v.dt1 * pt.ratio2).min(0.49);
                    let fc = pt.cutoff * (pt.fenv * v.env.level + pt.wobble * lfo).exp2();
                    v.filt.set(fc, pt.reso, sr);
                }
                v.ctl = v.ctl.wrapping_add(1);

                let mut s = osc(pt.w1, v.ph1, v.dt1) + pt.mix * osc(pt.w2, v.ph2, v.dt2);
                if pt.sub > 0.0 {
                    s += pt.sub * (v.phs * TAU).sin();
                }
                if pt.noise > 0.0 {
                    self.rng ^= self.rng << 13;
                    self.rng ^= self.rng >> 17;
                    self.rng ^= self.rng << 5;
                    s += pt.noise * (self.rng as f32 / u32::MAX as f32 * 2.0 - 1.0);
                }
                let env = v.env.next();
                *o += v.filt.lp(s * 0.5) * env * v.gain;

                v.ph1 = (v.ph1 + v.dt1).fract();
                v.ph2 = (v.ph2 + v.dt2).fract();
                v.phs = (v.phs + v.dt1 * 0.5).fract();
                if v.env.is_idle() {
                    v.active = false;
                    break;
                }
            }
        }
        self.lfo_phase = (lfo0 + pt.lfo_inc * n as f32).fract();
        for o in out.iter_mut() {
            *o *= 0.35;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::defaults;

    #[test]
    fn note_produces_sound_and_releases() {
        let mut s = Synth::new(48000.0);
        s.prepare(&defaults());
        s.note_on(60, 100);
        let mut buf = vec![0.0; 4800];
        s.render(&mut buf);
        let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.05, "peak {peak}");
        s.note_off(60);
        for _ in 0..20 {
            s.render(&mut buf);
        }
        assert_eq!(s.active_voices(), 0);
    }

    #[test]
    fn sustain_holds_notes() {
        let mut s = Synth::new(48000.0);
        s.prepare(&defaults());
        s.set_sustain(true);
        s.note_on(60, 100);
        s.note_off(60);
        let mut buf = vec![0.0; 48000];
        s.render(&mut buf);
        assert_eq!(s.active_voices(), 1);
        s.set_sustain(false);
        s.render(&mut buf);
        assert_eq!(s.active_voices(), 0);
    }

    #[test]
    fn voice_stealing_caps_polyphony() {
        let mut s = Synth::new(48000.0);
        s.prepare(&defaults());
        for n in 0..20 {
            s.note_on(40 + n, 100);
        }
        assert_eq!(s.active_voices(), MAX_VOICES);
    }
}
