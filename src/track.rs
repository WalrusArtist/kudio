//! Loop tracks as seen by the audio thread.
//!
//! A track is built on the UI thread (where allocation is fine), boxed and
//! handed to the engine through the command queue. It never allocates while
//! playing.

use crate::drums::{DrumVoice, Pattern};
use crate::dsp::{Fx, PitchShifter};
use crate::params::{Params, p};
use crate::synth::Synth;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Synth,
    Voice,
    Drum,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Synth => "synth",
            Kind::Voice => "voice",
            Kind::Drum => "drum",
        }
    }
}

/// A note-level event inside a synth loop. `at` is the frame offset inside the loop.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeqEvent {
    pub at: u64,
    #[serde(flatten)]
    pub ev: Ev,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Ev {
    On { note: u8, vel: u8 },
    Off { note: u8 },
    Sustain { down: bool },
}

impl SeqEvent {
    /// Playback order: by time, and at the same instant releases come before
    /// presses so a note that ends and restarts on the same frame retriggers.
    pub fn order_key(&self) -> (u64, u8) {
        let rank = match self.ev {
            Ev::Off { .. } => 0,
            Ev::Sustain { down: false } => 1,
            Ev::Sustain { down: true } => 2,
            Ev::On { .. } => 3,
        };
        (self.at, rank)
    }
}

#[allow(clippy::large_enum_variant)] // tracks are always boxed
pub enum Source {
    Synth {
        synth: Synth,
        events: Arc<[SeqEvent]>,
        cursor: usize,
    },
    Voice {
        audio: Arc<[f32]>,
        shifter: PitchShifter,
    },
    /// A drum pattern: the loop holds `bars` bars and hits are computed from
    /// the `rate`/`offset` params, so there is no stored data.
    Drum { voice: DrumVoice, bars: u32 },
}

pub struct Track {
    pub id: u32,
    pub params: Params,
    pub len: u64,
    pub t0: u64,
    pub muted: bool,
    pub solo: bool,
    pub source: Source,
    pub fx: Fx,
    pos: u64,
}

impl Track {
    pub fn new_synth(
        id: u32,
        params: Params,
        len: u64,
        t0: u64,
        events: Arc<[SeqEvent]>,
        sr: f32,
    ) -> Self {
        let source = Source::Synth {
            synth: Synth::new(sr),
            events,
            cursor: 0,
        };
        Self::new(id, params, len, t0, source, sr)
    }

    pub fn new_voice(
        id: u32,
        params: Params,
        len: u64,
        t0: u64,
        audio: Arc<[f32]>,
        sr: f32,
    ) -> Self {
        let source = Source::Voice {
            audio,
            shifter: PitchShifter::new(sr),
        };
        Self::new(id, params, len, t0, source, sr)
    }

    pub fn new_drum(id: u32, params: Params, len: u64, t0: u64, bars: u32, sr: f32) -> Self {
        let source = Source::Drum {
            voice: DrumVoice::new(sr),
            bars: bars.max(1),
        };
        Self::new(id, params, len, t0, source, sr)
    }

    fn new(id: u32, params: Params, len: u64, t0: u64, source: Source, sr: f32) -> Self {
        Self {
            id,
            params,
            len: len.max(1),
            t0,
            muted: false,
            solo: false,
            source,
            fx: Fx::new(sr),
            pos: 0,
        }
    }

    /// Jump the playhead to where it should be at absolute time `clock`.
    pub fn seek(&mut self, clock: u64) {
        self.pos = (clock + self.len - self.t0 % self.len) % self.len;
        if let Source::Synth { events, cursor, .. } = &mut self.source {
            *cursor = events.partition_point(|e| e.at < self.pos);
        }
    }

    pub fn prepare(&mut self) {
        match &mut self.source {
            Source::Synth { synth, .. } => synth.prepare(&self.params),
            Source::Drum { voice, .. } => voice.prepare(&self.params),
            Source::Voice { .. } => {}
        }
        self.fx.prepare(&self.params);
    }

    /// Render the dry (pre-FX) mono signal for the next `out.len()` frames.
    pub fn render(&mut self, out: &mut [f32]) {
        let len = self.len;
        match &mut self.source {
            Source::Synth {
                synth,
                events,
                cursor,
            } => {
                let n = out.len();
                let mut i = 0;
                while i < n {
                    while let Some(e) = events.get(*cursor) {
                        if e.at != self.pos {
                            break;
                        }
                        match e.ev {
                            Ev::On { note, vel } => synth.note_on(note, vel),
                            Ev::Off { note } => synth.note_off(note),
                            Ev::Sustain { down } => synth.set_sustain(down),
                        }
                        *cursor += 1;
                    }
                    let mut seg = ((n - i) as u64).min(len - self.pos);
                    if let Some(e) = events.get(*cursor) {
                        seg = seg.min(e.at.saturating_sub(self.pos));
                        if seg == 0 {
                            // Out-of-order event (should not happen): skip it.
                            *cursor += 1;
                            continue;
                        }
                    }
                    let seg = seg as usize;
                    synth.render(&mut out[i..i + seg]);
                    i += seg;
                    self.pos += seg as u64;
                    if self.pos >= len {
                        self.pos = 0;
                        *cursor = 0;
                    }
                }
            }
            Source::Voice { audio, shifter } => {
                let reverse = self.params[p::REVERSE] >= 0.5;
                let pitch = self.params[p::PITCH];
                let alen = audio.len() as u64;
                for o in out.iter_mut() {
                    let idx = if reverse {
                        len - 1 - self.pos
                    } else {
                        self.pos
                    };
                    let x = if idx < alen { audio[idx as usize] } else { 0.0 };
                    *o = shifter.process(x, pitch);
                    self.pos += 1;
                    if self.pos >= len {
                        self.pos = 0;
                    }
                }
            }
            Source::Drum { voice, bars } => {
                // Recomputed every block so rate/offset changes apply at once.
                let pat = Pattern::new(len, *bars, &self.params);
                let mut next = pat.next_hit(self.pos);
                let n = out.len();
                let mut i = 0;
                while i < n {
                    if self.pos == next {
                        voice.trigger();
                        next = pat.next_hit(self.pos + 1);
                    }
                    // `next > pos` here, and `next >= len` means no hit this pass.
                    let seg = ((n - i) as u64).min(len - self.pos).min(next - self.pos) as usize;
                    voice.render(&mut out[i..i + seg]);
                    i += seg;
                    self.pos += seg as u64;
                    if self.pos >= len {
                        self.pos = 0;
                        next = pat.next_hit(0);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::defaults;

    #[test]
    fn seek_handles_t0_after_clock() {
        let audio: Arc<[f32]> = vec![0.0; 100].into();
        let mut t = Track::new_voice(1, defaults(), 100, 250, audio, 48000.0);
        t.seek(0);
        // 250 % 100 = 50 -> at clock 0 we are 50 frames into the loop.
        assert_eq!(t.pos, 50);
        t.seek(260);
        assert_eq!(t.pos, 10);
    }

    #[test]
    fn synth_track_plays_events_and_wraps() {
        let events: Arc<[SeqEvent]> = vec![
            SeqEvent {
                at: 0,
                ev: Ev::On { note: 60, vel: 100 },
            },
            SeqEvent {
                at: 500,
                ev: Ev::Off { note: 60 },
            },
        ]
        .into();
        let mut t = Track::new_synth(1, defaults(), 1000, 0, events, 48000.0);
        t.prepare();
        t.seek(0);
        let mut buf = vec![0.0; 256];
        t.render(&mut buf);
        assert!(buf.iter().any(|x| x.abs() > 0.0));
        for _ in 0..10 {
            t.render(&mut buf);
        }
        // 11 * 256 = 2816 frames -> wrapped twice, now at 816.
        assert_eq!(t.pos, 2816 % 1000);
    }

    #[test]
    fn drum_track_hits_on_the_beat() {
        let mut prm = defaults();
        prm[p::DRUM] = 0.0;
        prm[p::RATE] = 2.0; // 1/4: four hits per bar
        // 1 bar = 4000 frames, so a beat is 1000 frames.
        let mut t = Track::new_drum(1, prm, 4000, 0, 1, 48000.0);
        t.prepare();
        t.seek(500);
        let mut buf = vec![0.0; 1000];
        t.render(&mut buf);
        // Silent until the beat at frame 1000 of the loop (index 500 here).
        assert!(buf[..500].iter().all(|x| *x == 0.0));
        assert!(buf[500..].iter().any(|x| x.abs() > 0.01));
        assert_eq!(t.pos, 1500);
        // Wraps around the loop without getting stuck.
        for _ in 0..10 {
            t.render(&mut buf);
        }
        assert_eq!(t.pos, (1500 + 10_000) % 4000);
    }

    #[test]
    fn event_json_shape() {
        let e = SeqEvent {
            at: 12,
            ev: Ev::On { note: 60, vel: 90 },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"at":12,"type":"on","note":60,"vel":90}"#);
        let back: SeqEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
