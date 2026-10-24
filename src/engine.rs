//! The real-time audio engine.
//!
//! `Engine` is moved into the audio output callback and owns every sound
//! source. It never locks and never allocates: the UI talks to it through
//! lock-free ring buffers (`rtrb`), and anything that must be freed is sent
//! back to the UI thread as an [`Event::Garbage`].

use crate::dsp::{Fx, PitchShifter, Reverb, Smooth, soft_clip};
use crate::params::{self, Params, p};
use crate::synth::Synth;
use crate::track::{Ev, Kind, SeqEvent, Track};
use rtrb::{Consumer, Producer, RingBuffer};

pub const MAX_BLOCK: usize = 1024;
pub const MAX_TRACKS: usize = 64;

/// Something whose parameters can be changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    LiveSynth,
    LiveVoice,
    /// Template for new drum tracks (UI only: the engine has no live drums).
    LiveDrums,
    Master,
    Track(u32),
}

/// UI/MIDI thread -> audio thread.
pub enum Cmd {
    NoteOn(u8, u8),
    NoteOff(u8),
    Sustain(bool),
    Bend(f32),
    ModWheel(f32),
    Panic,
    /// Momentary pitch offset for the live voice (piano mode in the voice module).
    KeyPitch(f32),
    SetParam(Target, usize, f32),
    Arm {
        kind: Kind,
        wait_for_note: bool,
    },
    StopRec,
    CancelRec,
    AddTrack(Box<Track>),
    /// Add a track whose loop starts right now (`t0` = current clock);
    /// answered with [`Event::TrackStarted`].
    AddTrackNow(Box<Track>),
    RemoveTrack(u32),
    Mute(u32, bool),
    Solo(u32, bool),
    TakeStart(Box<Producer<f32>>),
    TakeStop,
}

/// Audio thread -> UI thread.
pub enum Event {
    RecStarted {
        kind: Kind,
        t0: u64,
    },
    /// A note-level event captured while recording a synth loop (`at` = frames since t0).
    #[allow(clippy::enum_variant_names)]
    RecEvent(SeqEvent),
    RecStopped {
        kind: Kind,
        t0: u64,
        end: u64,
    },
    RecCancelled,
    /// A track added with [`Cmd::AddTrackNow`] started at absolute frame `t0`.
    TrackStarted {
        id: u32,
        t0: u64,
    },
    /// A track or buffer the engine no longer needs; dropped on the UI thread.
    Garbage(Box<Track>),
    GarbageTake(Box<Producer<f32>>),
    Meter {
        clock: u64,
        out: f32,
        mic: f32,
        voices: usize,
    },
    Overflow(&'static str),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rec {
    Idle,
    Armed(Kind),
    Recording(Kind, u64),
}

/// The UI-side ends of the engine's queues.
pub struct EngineLink {
    pub cmd: Producer<Cmd>,
    /// Second command queue reserved for the MIDI thread (taken once).
    pub midi_cmd: Option<Producer<Cmd>>,
    pub events: Consumer<Event>,
    pub capture: Consumer<f32>,
}

pub struct Engine {
    sr: f32,
    clock: u64,
    ui_rx: Consumer<Cmd>,
    midi_rx: Consumer<Cmd>,
    ev_tx: Producer<Event>,
    mic_rx: Option<Consumer<f32>>,
    cap_tx: Producer<f32>,

    synth: Synth,
    synth_params: Params,
    synth_fx: Fx,

    voice_params: Params,
    voice_fx: Fx,
    voice_shift: PitchShifter,
    key_pitch: f32,

    master: Params,
    reverb: Reverb,
    volume: Smooth,

    // Boxed on purpose: tracks are built on the UI thread and moved in/out of
    // the audio thread as a pointer, so adding/removing never copies ~1 KB.
    #[allow(clippy::vec_box)]
    tracks: Vec<Box<Track>>,
    rec: Rec,
    take: Option<Box<Producer<f32>>>,

    l: Vec<f32>,
    r: Vec<f32>,
    rev: Vec<f32>,
    buf: Vec<f32>,
    vbuf: Vec<f32>,

    peak_out: f32,
    peak_mic: f32,
    meter_ctr: u64,
    take_overflow: bool,
    cap_overflow: bool,
}

impl Engine {
    pub fn new(sr: u32, mic_rx: Option<Consumer<f32>>) -> (Self, EngineLink) {
        let srf = sr as f32;
        let (cmd_tx, ui_rx) = RingBuffer::new(4096);
        let (midi_tx, midi_rx) = RingBuffer::new(1024);
        let (ev_tx, ev_rx) = RingBuffer::new(16384);
        // Two seconds of mono capture; the UI drains it every few milliseconds.
        let (cap_tx, cap_rx) = RingBuffer::new(sr as usize * 2);
        let engine = Self {
            sr: srf,
            clock: 0,
            ui_rx,
            midi_rx,
            ev_tx,
            mic_rx,
            cap_tx,
            synth: Synth::new(srf),
            synth_params: params::defaults(),
            synth_fx: Fx::new(srf),
            voice_params: params::defaults(),
            voice_fx: Fx::new(srf),
            voice_shift: PitchShifter::new(srf),
            key_pitch: 0.0,
            master: params::defaults(),
            reverb: Reverb::new(srf),
            volume: Smooth::new(0.0, 20.0, srf),
            tracks: Vec::with_capacity(MAX_TRACKS),
            rec: Rec::Idle,
            take: None,
            l: vec![0.0; MAX_BLOCK],
            r: vec![0.0; MAX_BLOCK],
            rev: vec![0.0; MAX_BLOCK],
            buf: vec![0.0; MAX_BLOCK],
            vbuf: vec![0.0; MAX_BLOCK],
            peak_out: 0.0,
            peak_mic: 0.0,
            meter_ctr: 0,
            take_overflow: false,
            cap_overflow: false,
        };
        let link = EngineLink {
            cmd: cmd_tx,
            midi_cmd: Some(midi_tx),
            events: ev_rx,
            capture: cap_rx,
        };
        (engine, link)
    }

    fn emit(&mut self, ev: Event) {
        // If the UI is not keeping up the event is dropped (and, for garbage,
        // freed here). Never block the audio thread.
        let _ = self.ev_tx.push(ev);
    }

    fn start_rec(&mut self, kind: Kind) {
        self.rec = Rec::Recording(kind, self.clock);
        self.cap_overflow = false;
        self.emit(Event::RecStarted {
            kind,
            t0: self.clock,
        });
    }

    fn record(&mut self, ev: Ev) {
        match self.rec {
            Rec::Armed(Kind::Synth) if matches!(ev, Ev::On { .. }) => {
                self.start_rec(Kind::Synth);
                self.emit(Event::RecEvent(SeqEvent { at: 0, ev }));
            }
            Rec::Recording(Kind::Synth, t0) => {
                self.emit(Event::RecEvent(SeqEvent {
                    at: self.clock - t0,
                    ev,
                }));
            }
            _ => {}
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::NoteOn(n, v) => {
                self.synth.note_on(n, v);
                self.record(Ev::On { note: n, vel: v });
            }
            Cmd::NoteOff(n) => {
                self.synth.note_off(n);
                self.record(Ev::Off { note: n });
            }
            Cmd::Sustain(down) => {
                self.synth.set_sustain(down);
                self.record(Ev::Sustain { down });
            }
            Cmd::Bend(semis) => self.synth.set_bend(semis),
            Cmd::ModWheel(v) => self.synth.set_mod(v),
            Cmd::Panic => {
                self.synth.all_off();
                self.key_pitch = 0.0;
            }
            Cmd::KeyPitch(semis) => self.key_pitch = semis,
            Cmd::SetParam(target, idx, v) => {
                if idx >= params::NUM {
                    return;
                }
                match target {
                    Target::LiveSynth => self.synth_params[idx] = v,
                    Target::LiveVoice => self.voice_params[idx] = v,
                    Target::LiveDrums => {}
                    Target::Master => self.master[idx] = v,
                    Target::Track(id) => {
                        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                            t.params[idx] = v;
                        }
                    }
                }
            }
            Cmd::Arm {
                kind,
                wait_for_note,
            } => {
                if self.rec == Rec::Idle {
                    if kind == Kind::Synth && wait_for_note {
                        self.rec = Rec::Armed(kind);
                    } else {
                        self.start_rec(kind);
                    }
                }
            }
            Cmd::StopRec => match self.rec {
                Rec::Recording(kind, t0) => {
                    self.rec = Rec::Idle;
                    self.emit(Event::RecStopped {
                        kind,
                        t0,
                        end: self.clock,
                    });
                }
                Rec::Armed(_) => {
                    self.rec = Rec::Idle;
                    self.emit(Event::RecCancelled);
                }
                Rec::Idle => {}
            },
            Cmd::CancelRec => {
                if self.rec != Rec::Idle {
                    self.rec = Rec::Idle;
                    self.emit(Event::RecCancelled);
                }
            }
            Cmd::AddTrack(t) => {
                self.add_track(t);
            }
            Cmd::AddTrackNow(mut t) => {
                t.t0 = self.clock;
                let (id, t0) = (t.id, t.t0);
                if self.add_track(t) {
                    self.emit(Event::TrackStarted { id, t0 });
                }
            }
            Cmd::RemoveTrack(id) => {
                if let Some(i) = self.tracks.iter().position(|t| t.id == id) {
                    let t = self.tracks.remove(i);
                    self.emit(Event::Garbage(t));
                }
            }
            Cmd::Mute(id, m) => {
                if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                    t.muted = m;
                }
            }
            Cmd::Solo(id, s) => {
                if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                    t.solo = s;
                }
            }
            Cmd::TakeStart(prod) => {
                self.take_overflow = false;
                if let Some(old) = self.take.replace(prod) {
                    self.emit(Event::GarbageTake(old));
                }
            }
            Cmd::TakeStop => {
                if let Some(old) = self.take.take() {
                    self.emit(Event::GarbageTake(old));
                }
            }
        }
    }

    fn add_track(&mut self, mut t: Box<Track>) -> bool {
        if self.tracks.len() < MAX_TRACKS {
            t.seek(self.clock);
            t.prepare();
            self.tracks.push(t);
            true
        } else {
            self.emit(Event::Overflow("track limit reached"));
            self.emit(Event::Garbage(t));
            false
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.midi_rx.pop() {
            self.handle(cmd);
        }
        while let Ok(cmd) = self.ui_rx.pop() {
            self.handle(cmd);
        }
    }

    /// Fill an interleaved output buffer. Called from the audio callback.
    pub fn process<T>(&mut self, out: &mut [T], channels: usize)
    where
        T: cpal::Sample + cpal::FromSample<f32>,
    {
        self.drain_commands();
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.render_block(n);
            for i in 0..n {
                let frame = &mut out[(done + i) * channels..(done + i + 1) * channels];
                if channels == 1 {
                    frame[0] = T::from_sample(0.5 * (self.l[i] + self.r[i]));
                } else {
                    frame[0] = T::from_sample(self.l[i]);
                    frame[1] = T::from_sample(self.r[i]);
                    for s in &mut frame[2..] {
                        *s = T::EQUILIBRIUM;
                    }
                }
            }
            done += n;
        }
    }

    fn render_voice(&mut self, n: usize) {
        let vb = &mut self.vbuf[..n];
        vb.fill(0.0);
        if let Some(rx) = &mut self.mic_rx {
            // Input and output clocks drift apart; keep mic latency bounded.
            let max_backlog = (self.sr * 0.03) as usize + n;
            let avail = rx.slots();
            if avail > max_backlog + n
                && let Ok(c) = rx.read_chunk(avail - max_backlog)
            {
                c.commit_all();
            }
            let k = rx.slots().min(n);
            if let Ok(c) = rx.read_chunk(k) {
                let (a, b) = c.as_slices();
                vb[..a.len()].copy_from_slice(a);
                vb[a.len()..a.len() + b.len()].copy_from_slice(b);
                c.commit_all();
            }
        }
        let gain = self.voice_params[p::INGAIN];
        let pitch = (self.voice_params[p::PITCH] + self.key_pitch).clamp(-24.0, 24.0);
        for x in vb.iter_mut() {
            *x *= gain;
            self.peak_mic = self.peak_mic.max(x.abs());
            *x = self.voice_shift.process(*x, pitch);
        }
        if let Rec::Recording(Kind::Voice, _) = self.rec {
            let k = self.cap_tx.slots().min(n);
            if let Ok(c) = self.cap_tx.write_chunk_uninit(k) {
                c.fill_from_iter(vb.iter().copied());
            }
            if k < n && !self.cap_overflow {
                self.cap_overflow = true;
                let _ = self
                    .ev_tx
                    .push(Event::Overflow("voice capture buffer full, audio lost"));
            }
        }
        if self.voice_params[p::MONITOR] >= 0.5 {
            self.voice_fx.prepare(&self.voice_params);
            self.voice_fx.process(
                vb,
                true,
                &mut self.l[..n],
                &mut self.r[..n],
                &mut self.rev[..n],
            );
        }
    }

    fn render_block(&mut self, n: usize) {
        self.l[..n].fill(0.0);
        self.r[..n].fill(0.0);
        self.rev[..n].fill(0.0);

        // Live synth.
        self.synth.prepare(&self.synth_params);
        self.synth.render(&mut self.buf[..n]);
        self.synth_fx.prepare(&self.synth_params);
        self.synth_fx.process(
            &self.buf[..n],
            true,
            &mut self.l[..n],
            &mut self.r[..n],
            &mut self.rev[..n],
        );

        // Live voice (microphone).
        self.render_voice(n);

        // Loop tracks.
        let any_solo = self.tracks.iter().any(|t| t.solo);
        for t in self.tracks.iter_mut() {
            t.prepare();
            t.render(&mut self.buf[..n]);
            let audible = !t.muted && (!any_solo || t.solo);
            t.fx.process(
                &self.buf[..n],
                audible,
                &mut self.l[..n],
                &mut self.r[..n],
                &mut self.rev[..n],
            );
        }

        // Master: reverb return, volume, limiter.
        self.reverb
            .set(self.master[p::ROOMSIZE], self.master[p::DAMP]);
        let vol_target = self.master[p::VOLUME];
        for i in 0..n {
            let (rl, rr) = self.reverb.process(self.rev[i]);
            let g = self.volume.next(vol_target);
            self.l[i] = soft_clip((self.l[i] + rl) * g);
            self.r[i] = soft_clip((self.r[i] + rr) * g);
            self.peak_out = self.peak_out.max(self.l[i].abs()).max(self.r[i].abs());
        }

        // Session recording ("take").
        if let Some(tx) = &mut self.take {
            if tx.slots() >= 2 * n {
                if let Ok(c) = tx.write_chunk_uninit(2 * n) {
                    let (l, r) = (&self.l[..n], &self.r[..n]);
                    c.fill_from_iter(l.iter().zip(r).flat_map(|(a, b)| [*a, *b]));
                }
            } else if !self.take_overflow {
                self.take_overflow = true;
                let _ = self
                    .ev_tx
                    .push(Event::Overflow("session recording buffer full, audio lost"));
            }
        }

        self.clock += n as u64;
        self.meter_ctr += n as u64;
        if self.meter_ctr as f32 >= self.sr / 15.0 {
            self.meter_ctr = 0;
            let voices = self.synth.active_voices();
            let ev = Event::Meter {
                clock: self.clock,
                out: self.peak_out,
                mic: self.peak_mic,
                voices,
            };
            self.emit(ev);
            self.peak_out = 0.0;
            self.peak_mic = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(engine: &mut Engine, frames: usize) {
        let mut out = vec![0.0f32; frames * 2];
        engine.process(&mut out, 2);
    }

    fn drain(link: &mut EngineLink) -> Vec<Event> {
        let mut v = Vec::new();
        while let Ok(e) = link.events.pop() {
            v.push(e);
        }
        v
    }

    #[test]
    fn synth_recording_waits_for_first_note() {
        let (mut e, mut link) = Engine::new(48000, None);
        link.cmd
            .push(Cmd::Arm {
                kind: Kind::Synth,
                wait_for_note: true,
            })
            .ok();
        run(&mut e, 512);
        assert!(
            !drain(&mut link)
                .iter()
                .any(|e| matches!(e, Event::RecStarted { .. }))
        );
        link.cmd.push(Cmd::NoteOn(60, 100)).ok();
        run(&mut e, 512);
        link.cmd.push(Cmd::NoteOff(60)).ok();
        run(&mut e, 512);
        link.cmd.push(Cmd::StopRec).ok();
        run(&mut e, 512);
        let evs = drain(&mut link);
        let t0 = evs
            .iter()
            .find_map(|e| {
                if let Event::RecStarted { t0, .. } = e {
                    Some(*t0)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(t0, 512);
        let notes: Vec<_> = evs
            .iter()
            .filter_map(|e| {
                if let Event::RecEvent(s) = e {
                    Some(*s)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].at, 512);
        assert!(
            evs.iter()
                .any(|e| matches!(e, Event::RecStopped { end: 1536, .. }))
        );
    }

    #[test]
    fn voice_recording_captures_samples() {
        let (mut mic_tx, mic_rx) = RingBuffer::new(48000);
        let (mut e, mut link) = Engine::new(48000, Some(mic_rx));
        link.cmd
            .push(Cmd::Arm {
                kind: Kind::Voice,
                wait_for_note: false,
            })
            .ok();
        for _ in 0..256 {
            mic_tx.push(0.5).ok();
        }
        run(&mut e, 256);
        link.cmd.push(Cmd::StopRec).ok();
        run(&mut e, 256);
        let mut got = Vec::new();
        while let Ok(s) = link.capture.pop() {
            got.push(s);
        }
        assert_eq!(got.len(), 256);
        assert!((got[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn track_now_reports_its_start() {
        let (mut e, mut link) = Engine::new(48000, None);
        run(&mut e, 300);
        let t = Track::new_drum(3, params::defaults(), 4800, 0, 1, 48000.0);
        link.cmd.push(Cmd::AddTrackNow(Box::new(t))).ok();
        run(&mut e, 64);
        assert!(
            drain(&mut link)
                .iter()
                .any(|e| matches!(e, Event::TrackStarted { id: 3, t0: 300 }))
        );
    }

    #[test]
    fn tracks_are_returned_as_garbage() {
        let (mut e, mut link) = Engine::new(48000, None);
        let t = Track::new_voice(
            7,
            params::defaults(),
            100,
            0,
            vec![0.0; 100].into(),
            48000.0,
        );
        link.cmd.push(Cmd::AddTrack(Box::new(t))).ok();
        run(&mut e, 64);
        link.cmd.push(Cmd::RemoveTrack(7)).ok();
        run(&mut e, 64);
        assert!(
            drain(&mut link)
                .iter()
                .any(|e| matches!(e, Event::Garbage(t) if t.id == 7))
        );
    }
}
