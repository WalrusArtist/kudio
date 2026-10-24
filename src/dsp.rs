//! Small, allocation-free (after construction) DSP building blocks.

use crate::params::{Params, p};
use std::f32::consts::{PI, TAU};

/// One-pole parameter smoother, removes zipper noise when values jump.
#[derive(Clone, Copy)]
pub struct Smooth {
    pub v: f32,
    coef: f32,
}

impl Smooth {
    pub fn new(v: f32, ms: f32, sr: f32) -> Self {
        Self {
            v,
            coef: 1.0 - (-1.0 / (ms * 0.001 * sr)).exp(),
        }
    }
    #[inline]
    pub fn next(&mut self, target: f32) -> f32 {
        self.v += (target - self.v) * self.coef;
        self.v
    }
}

/// Topology-preserving state variable filter (Cytomic / A. Simper).
#[derive(Clone, Copy, Default)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

impl Svf {
    /// `res` in 0..1.
    pub fn set(&mut self, fc: f32, res: f32, sr: f32) {
        let fc = fc.clamp(10.0, sr * 0.45);
        let g = (PI * fc / sr).tan();
        self.k = 2.0 - 1.96 * res.clamp(0.0, 1.0);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }
    #[inline]
    fn tick(&mut self, v0: f32) -> (f32, f32) {
        let v3 = v0 - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v1, v2)
    }
    #[inline]
    pub fn lp(&mut self, x: f32) -> f32 {
        self.tick(x).1
    }
    #[inline]
    pub fn hp(&mut self, x: f32) -> f32 {
        let (band, low) = self.tick(x);
        x - self.k * band - low
    }
    pub fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Linear-attack, exponential decay/release ADSR envelope.
#[derive(Clone, Copy)]
pub struct Adsr {
    stage: Stage,
    pub level: f32,
    a_inc: f32,
    d_coef: f32,
    sustain: f32,
    r_coef: f32,
}

impl Default for Adsr {
    fn default() -> Self {
        Self {
            stage: Stage::Idle,
            level: 0.0,
            a_inc: 1.0,
            d_coef: 0.0,
            sustain: 1.0,
            r_coef: 0.0,
        }
    }
}

impl Adsr {
    pub fn set(&mut self, a: f32, d: f32, s: f32, r: f32, sr: f32) {
        self.a_inc = 1.0 / (a.max(0.0005) * sr);
        self.d_coef = (-5.0 / (d.max(0.001) * sr)).exp();
        self.sustain = s;
        self.r_coef = (-5.0 / (r.max(0.001) * sr)).exp();
    }
    pub fn gate_on(&mut self) {
        self.stage = Stage::Attack;
    }
    pub fn gate_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }
    pub fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }
    #[inline]
    pub fn next(&mut self) -> f32 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += self.a_inc;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = self.sustain + (self.level - self.sustain) * self.d_coef;
                if (self.level - self.sustain).abs() < 1e-4 {
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.level = self.sustain,
            Stage::Release => {
                self.level *= self.r_coef;
                if self.level < 1e-4 {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

/// PolyBLEP residual for band-limited saw/square edges.
#[inline]
pub fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        t + t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

/// Band-limited oscillator sample for phase `t` (0..1) and increment `dt`.
#[inline]
pub fn osc(wave: u8, t: f32, dt: f32) -> f32 {
    match wave {
        0 => (t * TAU).sin(),
        1 => 1.0 - 4.0 * (t - 0.5).abs(),
        2 => 2.0 * t - 1.0 - poly_blep(t, dt),
        _ => {
            let sq = if t < 0.5 { 1.0 } else { -1.0 };
            sq + poly_blep(t, dt) - poly_blep((t + 0.5).fract(), dt)
        }
    }
}

/// Mono delay line with smoothed, interpolated read position.
pub struct Delay {
    buf: Vec<f32>,
    w: usize,
    time: Smooth,
}

impl Delay {
    pub fn new(max_seconds: f32, sr: f32) -> Self {
        Self {
            buf: vec![0.0; (max_seconds * sr) as usize + 4],
            w: 0,
            time: Smooth::new(sr * 0.375, 80.0, sr),
        }
    }
    #[inline]
    pub fn process(&mut self, x: f32, time_samples: f32, fb: f32, mix: f32) -> f32 {
        let n = self.buf.len();
        let d = self.time.next(time_samples).clamp(1.0, (n - 3) as f32);
        let rp = self.w as f32 + n as f32 - d;
        let i = rp as usize;
        let frac = rp - i as f32;
        let a = self.buf[i % n];
        let b = self.buf[(i + 1) % n];
        let delayed = a + (b - a) * frac;
        self.buf[self.w] = x + delayed * fb;
        self.w = (self.w + 1) % n;
        x + delayed * mix
    }
}

/// Delay-line pitch shifter: two read taps sweep through a short window and
/// are cross-faded with a sine window. Cheap, a bit grainy — very "lo-fi
/// sampler", which suits a looper.
pub struct PitchShifter {
    buf: Vec<f32>,
    mask: usize,
    w: usize,
    phase: f32,
    window: f32,
    wet: Smooth,
}

impl PitchShifter {
    pub fn new(sr: f32) -> Self {
        let window = 0.04 * sr;
        let size = ((window as usize) + 8).next_power_of_two();
        Self {
            buf: vec![0.0; size],
            mask: size - 1,
            w: 0,
            phase: 0.0,
            window,
            wet: Smooth::new(0.0, 15.0, sr),
        }
    }

    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let rp = self.w as f32 - delay + self.buf.len() as f32;
        let i = rp as usize;
        let frac = rp - i as f32;
        let a = self.buf[i & self.mask];
        let b = self.buf[(i + 1) & self.mask];
        a + (b - a) * frac
    }

    #[inline]
    pub fn process(&mut self, x: f32, semis: f32) -> f32 {
        self.buf[self.w] = x;
        let active = semis.abs() > 0.01;
        let wet = self.wet.next(if active { 1.0 } else { 0.0 });
        let out = if wet > 1e-4 {
            let ratio = (semis / 12.0).exp2();
            self.phase = (self.phase + (1.0 - ratio) / self.window).rem_euclid(1.0);
            let p2 = (self.phase + 0.5).fract();
            let shifted = self.read(self.phase * self.window + 1.0) * (PI * self.phase).sin()
                + self.read(p2 * self.window + 1.0) * (PI * p2).sin();
            x + (shifted - x) * wet
        } else {
            x
        };
        self.w = (self.w + 1) & self.mask;
        out
    }
}

struct Comb {
    buf: Vec<f32>,
    i: usize,
    store: f32,
}

impl Comb {
    #[inline]
    fn process(&mut self, x: f32, fb: f32, d1: f32) -> f32 {
        let out = self.buf[self.i];
        self.store = out * (1.0 - d1) + self.store * d1;
        self.buf[self.i] = x + self.store * fb;
        self.i = (self.i + 1) % self.buf.len();
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    i: usize,
}

impl Allpass {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let b = self.buf[self.i];
        self.buf[self.i] = x + b * 0.5;
        self.i = (self.i + 1) % self.buf.len();
        b - x
    }
}

/// Freeverb (Jezar's public-domain design), mono in / stereo out.
/// One instance lives on the master bus; sources feed it through a send.
pub struct Reverb {
    combs: [Vec<Comb>; 2],
    alls: [Vec<Allpass>; 2],
    fb: f32,
    damp: f32,
}

impl Reverb {
    pub fn new(sr: f32) -> Self {
        const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
        const ALLS: [usize; 4] = [556, 441, 341, 225];
        let scale = sr / 44100.0;
        let mk = |spread: usize| {
            let combs = COMBS
                .iter()
                .map(|&n| Comb {
                    buf: vec![0.0; ((n + spread) as f32 * scale) as usize],
                    i: 0,
                    store: 0.0,
                })
                .collect();
            let alls = ALLS
                .iter()
                .map(|&n| Allpass {
                    buf: vec![0.0; ((n + spread) as f32 * scale) as usize],
                    i: 0,
                })
                .collect();
            (combs, alls)
        };
        let (cl, al) = mk(0);
        let (cr, ar) = mk(23);
        Self {
            combs: [cl, cr],
            alls: [al, ar],
            fb: 0.84,
            damp: 0.2,
        }
    }

    pub fn set(&mut self, room: f32, damp: f32) {
        self.fb = 0.7 + room.clamp(0.0, 1.0) * 0.28;
        self.damp = damp.clamp(0.0, 1.0) * 0.4;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> (f32, f32) {
        let x = x * 0.015;
        let mut out = [0.0f32; 2];
        for ((o, combs), alls) in out.iter_mut().zip(&mut self.combs).zip(&mut self.alls) {
            let mut acc = 0.0;
            for c in combs {
                acc += c.process(x, self.fb, self.damp);
            }
            for a in alls {
                acc = a.process(acc);
            }
            *o = acc * 3.0;
        }
        (out[0], out[1])
    }
}

/// Per-source effect chain:
/// drive -> crush -> high-pass -> low-pass -> delay -> level/mute -> pan,
/// plus a post-fader send to the master reverb.
pub struct Fx {
    sr: f32,
    drive: f32,
    crush: f32,
    hold: f32,
    hold_ctr: f32,
    held: f32,
    q: f32,
    hp_on: bool,
    lp_on: bool,
    hp: Svf,
    lp: Svf,
    delay: Delay,
    dtime: f32,
    dfb: f32,
    dmix: f32,
    send: f32,
    gain: Smooth,
    pan: Smooth,
    level: f32,
    pan_target: f32,
}

impl Fx {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            drive: 0.0,
            crush: 0.0,
            hold: 1.0,
            hold_ctr: 0.0,
            held: 0.0,
            q: 32768.0,
            hp_on: false,
            lp_on: false,
            hp: Svf::default(),
            lp: Svf::default(),
            delay: Delay::new(1.0, sr),
            dtime: 0.375 * sr,
            dfb: 0.0,
            dmix: 0.0,
            send: 0.0,
            gain: Smooth::new(0.0, 10.0, sr),
            pan: Smooth::new(0.0, 20.0, sr),
            level: 0.8,
            pan_target: 0.0,
        }
    }

    /// Recompute coefficients from the parameter array (once per block).
    pub fn prepare(&mut self, prm: &Params) {
        self.drive = prm[p::DRIVE];
        self.crush = prm[p::CRUSH];
        self.hold = 1.0 + self.crush * self.crush * 30.0;
        self.q = (16.0 - self.crush * 13.0).exp2() * 0.5;
        self.hp_on = prm[p::HP] > 21.0;
        self.lp_on = prm[p::LP] < 19900.0;
        if self.hp_on {
            self.hp.set(prm[p::HP], 0.1, self.sr);
        }
        if self.lp_on {
            self.lp.set(prm[p::LP], 0.1, self.sr);
        }
        self.dtime = prm[p::DTIME] * 0.001 * self.sr;
        self.dfb = prm[p::DFB];
        self.dmix = prm[p::DMIX];
        self.send = prm[p::REVERB];
        self.level = prm[p::LEVEL];
        self.pan_target = prm[p::PAN];
    }

    /// Process a mono block and *add* the result into the stereo mix and reverb bus.
    pub fn process(
        &mut self,
        input: &[f32],
        audible: bool,
        l: &mut [f32],
        r: &mut [f32],
        rev: &mut [f32],
    ) {
        let target = if audible { self.level } else { 0.0 };
        let drive_g = 1.0 + 15.0 * self.drive;
        let drive_norm = 1.0 / drive_g.sqrt();
        for i in 0..input.len() {
            let mut x = input[i];
            if self.drive > 0.001 {
                x = (x * drive_g).tanh() * drive_norm;
            }
            if self.crush > 0.001 {
                self.hold_ctr += 1.0;
                if self.hold_ctr >= self.hold {
                    self.hold_ctr -= self.hold;
                    self.held = (x * self.q).round() / self.q;
                }
                x = self.held;
            }
            if self.hp_on {
                x = self.hp.hp(x);
            }
            if self.lp_on {
                x = self.lp.lp(x);
            }
            x = self.delay.process(x, self.dtime, self.dfb, self.dmix);
            let g = self.gain.next(target);
            x *= g;
            let pan = self.pan.next(self.pan_target);
            let angle = (pan + 1.0) * PI * 0.25;
            l[i] += x * angle.cos();
            r[i] += x * angle.sin();
            rev[i] += x * self.send;
        }
    }
}

/// Gentle master limiter: transparent below 0.8, soft-knee above.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a < 0.8 {
        x
    } else {
        x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adsr_runs_full_cycle() {
        let mut e = Adsr::default();
        e.set(0.001, 0.01, 0.5, 0.01, 48000.0);
        e.gate_on();
        for _ in 0..2000 {
            e.next();
        }
        assert!((e.level - 0.5).abs() < 0.01);
        e.gate_off();
        for _ in 0..4000 {
            e.next();
        }
        assert!(e.is_idle());
    }

    #[test]
    fn svf_lowpass_passes_dc() {
        let mut f = Svf::default();
        f.set(1000.0, 0.0, 48000.0);
        let mut y = 0.0;
        for _ in 0..10000 {
            y = f.lp(1.0);
        }
        assert!((y - 1.0).abs() < 1e-3);
    }

    #[test]
    fn pitch_shifter_bypasses_at_zero() {
        let mut ps = PitchShifter::new(48000.0);
        for i in 0..100 {
            let x = (i as f32 * 0.1).sin();
            assert_eq!(ps.process(x, 0.0), x);
        }
    }

    #[test]
    fn soft_clip_is_bounded() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(10.0) <= 1.0);
        assert!(soft_clip(-10.0) >= -1.0);
    }
}
