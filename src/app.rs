//! The UI thread: keyboard handling, logging, track bookkeeping.
//!
//! The `App` owns the "model" (all parameters and the track list) and mirrors
//! every change to the audio engine by sending a [`Cmd`]. The engine reports
//! back through [`Event`]s (recording finished, meters, garbage to free).

use crate::dlog;
use crate::drums;
use crate::engine::{Cmd, EngineLink, Event, Target};
use crate::keymap;
use crate::midi::{Midi, MidiLog, Status};
use crate::params::{self, Params, Scope, p};
use crate::session::{self, Loaded, SaveInput, TakeWriter, TrackData, TrackModel};
use crate::term::{Cat, Term};
use crate::track::{Ev, Kind, SeqEvent, Track};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Module {
    Synth,
    Voice,
    Drums,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Piano,
    Control,
}

pub const BPM_MIN: f32 = 40.0;
pub const BPM_MAX: f32 = 300.0;

/// What control mode is pointed at. `Live` follows the active module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sel {
    Live,
    Master,
    Track(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecUi {
    Idle,
    Armed(Kind),
    Recording(Kind, u64),
}

struct Held {
    note: u8,
    module: Module,
    until: Option<Instant>,
}

pub struct App {
    pub term: Term,
    link: EngineLink,
    pub sr: u32,
    pub midi: Midi,
    midi_log: Receiver<MidiLog>,
    sys_log: Receiver<String>,
    sys_tx: Sender<String>,
    /// Terminal reports key releases (kitty keyboard protocol).
    pub enhanced: bool,
    pub mic_ok: bool,

    pub module: Module,
    pub mode: Mode,
    pub prompt: Option<String>,
    history: Vec<String>,
    hist_pos: Option<usize>,
    pub octave: i32,
    pub velocity: u8,
    held: HashMap<char, Held>,
    voice_keys: Vec<(char, f32)>,
    /// Note length used when the terminal cannot report key releases.
    pub gate: f32,
    pub verbose: bool,

    pub live_synth: Params,
    pub live_voice: Params,
    /// Template for new drum tracks ("live" target of the drums module).
    pub live_drums: Params,
    pub master: Params,
    pub tracks: Vec<TrackModel>,
    pub next_id: u32,
    pub loop_len: Option<u64>,
    pub loop_t0: u64,
    /// The loop phase (`loop_t0`) will be set by the next `TrackStarted`.
    phase_pending: bool,
    pub bpm: f32,
    taps: Vec<Instant>,
    /// Legacy terminals send key repeats as presses; used to ignore them on drum keys.
    last_drum_key: Option<(char, Instant)>,

    pub rec: RecUi,
    rec_events: Vec<SeqEvent>,
    rec_audio: Vec<f32>,

    pub sel: Sel,
    pub page: usize,
    pub cursor: usize,
    clipboard: Option<(Params, Scope)>,
    /// Track waiting for a second Delete press.
    pending_delete: Option<u32>,

    pub session: String,
    take: Option<TakeWriter>,
    finished_takes: Vec<TakeWriter>,
    clock: u64,
    meter_out: f32,
    meter_mic: f32,
    voices: usize,
    last_footer: Instant,
    pub quit: bool,
}

pub struct AppInit {
    pub link: EngineLink,
    pub sr: u32,
    pub midi: Midi,
    pub midi_log: Receiver<MidiLog>,
    pub sys_log: Receiver<String>,
    pub sys_tx: Sender<String>,
    pub enhanced: bool,
    pub mic_ok: bool,
    pub session: String,
}

impl App {
    pub fn new(init: AppInit) -> Self {
        Self {
            term: Term::new(),
            link: init.link,
            sr: init.sr,
            midi: init.midi,
            midi_log: init.midi_log,
            sys_log: init.sys_log,
            sys_tx: init.sys_tx,
            enhanced: init.enhanced,
            mic_ok: init.mic_ok,
            module: Module::Synth,
            mode: Mode::Piano,
            prompt: None,
            history: Vec::new(),
            hist_pos: None,
            octave: 3,
            velocity: 100,
            held: HashMap::new(),
            voice_keys: Vec::new(),
            gate: 0.35,
            verbose: false,
            live_synth: params::defaults(),
            live_voice: params::defaults(),
            live_drums: params::defaults(),
            master: params::defaults(),
            tracks: Vec::new(),
            next_id: 1,
            loop_len: None,
            loop_t0: 0,
            phase_pending: false,
            bpm: 120.0,
            taps: Vec::new(),
            last_drum_key: None,
            rec: RecUi::Idle,
            rec_events: Vec::new(),
            rec_audio: Vec::new(),
            sel: Sel::Live,
            page: 0,
            cursor: 0,
            clipboard: None,
            pending_delete: None,
            session: init.session,
            take: None,
            finished_takes: Vec::new(),
            clock: 0,
            meter_out: 0.0,
            meter_mic: 0.0,
            voices: 0,
            last_footer: Instant::now() - Duration::from_secs(1),
            quit: false,
        }
    }

    // ------------------------------------------------------------------ utils

    pub fn log(&mut self, cat: Cat, msg: impl AsRef<str>) {
        self.term.log(cat, msg);
    }

    pub fn send(&mut self, cmd: Cmd) {
        if self.link.cmd.push(cmd).is_err() {
            self.term
                .log(Cat::Warn, "audio engine queue full, command dropped");
        }
    }

    pub fn secs(&self, frames: u64) -> f32 {
        frames as f32 / self.sr as f32
    }

    pub fn track_index(&self, id: u32) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    pub fn live_target(&self) -> Target {
        match self.module {
            Module::Synth => Target::LiveSynth,
            Module::Voice => Target::LiveVoice,
            Module::Drums => Target::LiveDrums,
        }
    }

    pub fn sel_target(&self) -> Target {
        match self.sel {
            Sel::Live => self.live_target(),
            Sel::Master => Target::Master,
            Sel::Track(id) => Target::Track(id),
        }
    }

    pub fn scope_of(&self, t: Target) -> Option<Scope> {
        Some(match t {
            Target::LiveSynth => Scope::Synth { track: false },
            Target::LiveVoice => Scope::Voice { track: false },
            Target::LiveDrums => Scope::Drum { track: false },
            Target::Master => Scope::Master,
            Target::Track(id) => self.tracks.get(self.track_index(id)?)?.scope(),
        })
    }

    pub fn label_of(&self, t: Target) -> String {
        match t {
            Target::LiveSynth => "live synth".into(),
            Target::LiveVoice => "live voice".into(),
            Target::LiveDrums => "drum defaults (new drum tracks)".into(),
            Target::Master => "master".into(),
            Target::Track(id) => match self.track_index(id) {
                Some(i) => format!("track {} '{}'", i + 1, self.tracks[i].name),
                None => format!("track id {id} (deleted)"),
            },
        }
    }

    pub fn params_of(&self, t: Target) -> Option<&Params> {
        Some(match t {
            Target::LiveSynth => &self.live_synth,
            Target::LiveVoice => &self.live_voice,
            Target::LiveDrums => &self.live_drums,
            Target::Master => &self.master,
            Target::Track(id) => &self.tracks.get(self.track_index(id)?)?.params,
        })
    }

    fn params_mut(&mut self, t: Target) -> Option<&mut Params> {
        Some(match t {
            Target::LiveSynth => &mut self.live_synth,
            Target::LiveVoice => &mut self.live_voice,
            Target::LiveDrums => &mut self.live_drums,
            Target::Master => &mut self.master,
            Target::Track(id) => {
                let i = self.track_index(id)?;
                &mut self.tracks[i].params
            }
        })
    }

    /// Change one parameter in the model and in the engine, and log it.
    pub fn set_param(&mut self, t: Target, idx: usize, value: f32) -> Result<(), String> {
        let scope = self.scope_of(t).ok_or("no such target")?;
        let name = params::SPECS[idx].name;
        if !scope.applies(idx) {
            return Err(format!("'{name}' does not apply to {}", self.label_of(t)));
        }
        let v = params::clamp(idx, value);
        let slot = self.params_mut(t).ok_or("no such target")?;
        let old = slot[idx];
        slot[idx] = v;
        self.send(Cmd::SetParam(t, idx, v));
        let label = self.label_of(t);
        self.log(
            Cat::Ctrl,
            format!(
                "{label} · {name} {} → {}",
                params::format(idx, old),
                params::format(idx, v)
            ),
        );
        Ok(())
    }

    /// Push a whole parameter set to the engine (after load/paste/preset).
    pub fn sync_params(&mut self, t: Target) {
        if let Some(prm) = self.params_of(t).copied() {
            for (i, v) in prm.iter().enumerate() {
                self.send(Cmd::SetParam(t, i, *v));
            }
        }
    }

    pub fn build_track(&self, m: &TrackModel) -> Box<Track> {
        let sr = self.sr as f32;
        let mut t = match &m.data {
            TrackData::Synth(ev) => Track::new_synth(m.id, m.params, m.len, m.t0, ev.clone(), sr),
            TrackData::Voice(a) => Track::new_voice(m.id, m.params, m.len, m.t0, a.clone(), sr),
            TrackData::Drum { bars } => Track::new_drum(m.id, m.params, m.len, m.t0, *bars, sr),
        };
        t.muted = m.muted;
        t.solo = m.solo;
        Box::new(t)
    }

    pub fn add_track(&mut self, m: TrackModel) {
        self.push_track(m, false);
    }

    /// `now`: the loop starts at the engine's current clock (its real `t0`
    /// comes back in [`Event::TrackStarted`]).
    fn push_track(&mut self, m: TrackModel, now: bool) {
        let t = self.build_track(&m);
        self.send(if now {
            Cmd::AddTrackNow(t)
        } else {
            Cmd::AddTrack(t)
        });
        self.next_id = self.next_id.max(m.id + 1);
        self.tracks.push(m);
    }

    pub fn remove_track(&mut self, id: u32) -> Option<TrackModel> {
        let i = self.track_index(id)?;
        self.send(Cmd::RemoveTrack(id));
        if self.pending_delete == Some(id) {
            self.pending_delete = None;
        }
        if self.sel == Sel::Track(id) {
            self.sel = Sel::Live;
            self.page = 0;
            self.cursor = 0;
        }
        Some(self.tracks.remove(i))
    }

    pub fn apply_loaded(&mut self, l: Loaded) {
        for id in self.tracks.iter().map(|t| t.id).collect::<Vec<_>>() {
            self.remove_track(id);
        }
        for w in &l.warnings {
            self.term.log(Cat::Warn, w);
        }
        self.live_synth = l.live_synth;
        self.live_voice = l.live_voice;
        self.live_drums = l.live_drums;
        self.master = l.master;
        self.bpm = l.file.bpm.clamp(BPM_MIN, BPM_MAX);
        self.phase_pending = false;
        self.sync_params(Target::LiveSynth);
        self.sync_params(Target::LiveVoice);
        self.sync_params(Target::Master);
        let sr_ratio = self.sr as f64 / l.file.sample_rate.max(1) as f64;
        self.loop_len = l
            .file
            .loop_len
            .map(|v| (v as f64 * sr_ratio).round() as u64);
        self.loop_t0 = (l.file.loop_t0 as f64 * sr_ratio).round() as u64;
        for t in l.tracks {
            self.add_track(t);
        }
    }

    pub fn save(&mut self) {
        let input = SaveInput {
            sample_rate: self.sr,
            loop_len: self.loop_len,
            loop_t0: self.loop_t0,
            master: &self.master,
            live_synth: &self.live_synth,
            live_voice: &self.live_voice,
            live_drums: &self.live_drums,
            bpm: self.bpm,
            tracks: &self.tracks,
        };
        match session::save(&self.session, &input) {
            Ok(path) => self.log(Cat::Sess, format!("session saved → {}", path.display())),
            Err(e) => self.log(Cat::Err, format!("save failed: {e}")),
        }
    }

    // --------------------------------------------------------------- main loop

    pub fn run(&mut self) -> std::io::Result<()> {
        while !self.quit {
            if event::poll(Duration::from_millis(8))? {
                loop {
                    match event::read()? {
                        TermEvent::Key(k) => self.on_key(k),
                        TermEvent::Resize(w, _) => self.term.resize(w),
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            self.tick();
        }
        self.release_all();
        Ok(())
    }

    fn tick(&mut self) {
        self.drain_events();
        self.drain_capture();
        while let Ok(m) = self.midi_log.try_recv() {
            self.on_midi(m);
        }
        while let Ok(s) = self.sys_log.try_recv() {
            self.term.log(Cat::Sys, s);
        }
        if !self.enhanced {
            let now = Instant::now();
            let expired: Vec<char> = self
                .held
                .iter()
                .filter(|(_, h)| h.until.is_some_and(|u| u <= now))
                .map(|(c, _)| *c)
                .collect();
            for c in expired {
                dlog!("legacy gate expired: '{c}'");
                self.note_up(c);
            }
        }
        match self.midi.poll() {
            Some(Status::Connected(n)) => self.log(Cat::Midi, format!("🎹 connected: {n}")),
            Some(Status::Disconnected(n)) => self.log(
                Cat::Midi,
                format!("disconnected: {n} (will reconnect when plugged in)"),
            ),
            Some(Status::Error(e)) => self.log(Cat::Warn, e),
            None => {}
        }
        if self.last_footer.elapsed() >= Duration::from_millis(66) {
            self.refresh_footer();
        }
    }

    fn drain_capture(&mut self) {
        let keep = matches!(
            self.rec,
            RecUi::Armed(Kind::Voice) | RecUi::Recording(Kind::Voice, _)
        );
        let n = self.link.capture.slots();
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.link.capture.read_chunk(n) {
            if keep {
                let (a, b) = chunk.as_slices();
                self.rec_audio.extend_from_slice(a);
                self.rec_audio.extend_from_slice(b);
            }
            chunk.commit_all();
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.link.events.pop() {
            match ev {
                Event::RecStarted { kind, t0 } => {
                    let was_armed = matches!(self.rec, RecUi::Armed(_));
                    self.rec = RecUi::Recording(kind, t0);
                    if kind == Kind::Synth && was_armed && self.loop_len.is_none() {
                        self.log(
                            Cat::Loop,
                            "● first note — recording! press Space to close the loop",
                        );
                    }
                }
                Event::RecEvent(e) => self.rec_events.push(e),
                Event::TrackStarted { id, t0 } => {
                    dlog!("engine: track {id} started at frame {t0}");
                    if let Some(i) = self.track_index(id) {
                        self.tracks[i].t0 = t0;
                    }
                    if self.phase_pending {
                        self.phase_pending = false;
                        self.loop_t0 = t0;
                    }
                }
                Event::RecStopped { kind, t0, end } => {
                    self.drain_capture();
                    self.finish_recording(kind, t0, end);
                }
                Event::RecCancelled => {
                    self.rec = RecUi::Idle;
                    self.rec_events.clear();
                    self.rec_audio.clear();
                    self.log(Cat::Loop, "recording cancelled");
                }
                Event::Garbage(t) => drop(t),
                Event::GarbageTake(p) => drop(p),
                Event::Meter {
                    clock,
                    out,
                    mic,
                    voices,
                } => {
                    self.clock = clock;
                    self.meter_out = self.meter_out.max(out);
                    self.meter_mic = self.meter_mic.max(mic);
                    self.voices = voices;
                }
                Event::Overflow(msg) => self.log(Cat::Warn, msg),
            }
        }
    }

    // ------------------------------------------------------------- recording

    pub fn loop_toggle(&mut self) {
        match self.rec {
            RecUi::Idle => {
                let kind = match self.module {
                    Module::Synth => Kind::Synth,
                    Module::Voice => Kind::Voice,
                    Module::Drums => {
                        self.log(
                            Cat::Drum,
                            "drum tracks loop by themselves: b s h c start them · F1/F2 to record synth or voice",
                        );
                        return;
                    }
                };
                if kind == Kind::Voice && !self.mic_ok {
                    self.log(
                        Cat::Warn,
                        "no microphone available — cannot record a voice loop",
                    );
                    return;
                }
                if self.tracks.len() >= crate::engine::MAX_TRACKS {
                    self.log(Cat::Warn, "track limit reached; delete a track first");
                    return;
                }
                // The very first synth loop starts on the first note so the
                // loop length is exactly what you play.
                let wait = kind == Kind::Synth && self.loop_len.is_none();
                self.rec_events.clear();
                self.rec_audio.clear();
                self.rec = RecUi::Armed(kind);
                self.send(Cmd::Arm {
                    kind,
                    wait_for_note: wait,
                });
                let msg = match (kind, wait, self.loop_len) {
                    (Kind::Synth, true, _) => {
                        "◌ armed: synth loop starts with your first note".to_string()
                    }
                    (Kind::Synth, false, Some(l)) => {
                        format!(
                            "● recording synth loop (loop = {:.2} s, Space to finish)",
                            self.secs(l)
                        )
                    }
                    (Kind::Voice, _, Some(l)) => {
                        format!(
                            "● recording voice loop — sing! (loop = {:.2} s, Space to finish)",
                            self.secs(l)
                        )
                    }
                    (Kind::Voice, _, None) => {
                        "● recording voice loop — sing! Space to close the loop".to_string()
                    }
                    _ => "● recording".to_string(),
                };
                self.log(Cat::Loop, msg);
            }
            RecUi::Armed(_) | RecUi::Recording(..) => self.send(Cmd::StopRec),
        }
    }

    pub fn cancel_recording(&mut self) {
        if self.rec != RecUi::Idle {
            self.send(Cmd::CancelRec);
        }
    }

    fn finish_recording(&mut self, kind: Kind, t0: u64, end: u64) {
        self.rec = RecUi::Idle;
        let frames = end.saturating_sub(t0);
        let events = std::mem::take(&mut self.rec_events);
        let mut audio = std::mem::take(&mut self.rec_audio);
        if frames < self.sr as u64 / 10 {
            self.log(Cat::Warn, "loop too short (< 0.1 s), discarded");
            return;
        }
        if kind == Kind::Synth && !events.iter().any(|e| matches!(e.ev, Ev::On { .. })) {
            self.log(Cat::Warn, "no notes were played, loop discarded");
            return;
        }
        let first = self.loop_len.is_none();
        let (len, reps) = match self.loop_len {
            None => (frames, 1),
            Some(l) => {
                let n = ((frames as f64 / l as f64).round() as u64).max(1);
                (n * l, n)
            }
        };

        let data = match kind {
            Kind::Synth => TrackData::Synth(fold_events(events, frames, len).into()),
            Kind::Voice => {
                audio.resize(frames as usize, 0.0);
                TrackData::Voice(fold_audio(&audio, len as usize, self.sr).into())
            }
            Kind::Drum => return,
        };
        let params = match kind {
            Kind::Synth => self.live_synth,
            Kind::Voice => {
                // Pitch shifting is baked into the recording; FX stay tweakable.
                let mut p = self.live_voice;
                p[p::PITCH] = 0.0;
                p
            }
            Kind::Drum => self.live_drums,
        };
        if first {
            self.loop_len = Some(len);
            self.loop_t0 = t0;
        }
        let id = self.next_id;
        let model = TrackModel {
            id,
            name: format!("{}-{id}", kind.label()),
            kind,
            params,
            len,
            t0,
            muted: false,
            solo: false,
            data,
        };
        self.add_track(model);
        let n = self.tracks.len();
        let secs = self.secs(len);
        let detail = if first {
            format!("loop length set to {secs:.2} s")
        } else if reps > 1 {
            format!("{reps} × {:.2} s = {secs:.2} s", secs / reps as f32)
        } else {
            format!("{secs:.2} s")
        };
        self.log(
            Cat::Loop,
            format!(
                "■ track {n} '{}-{id}' recorded · {detail} · now looping",
                kind.label()
            ),
        );
    }

    pub fn toggle_take(&mut self) {
        if let Some(w) = self.take.take() {
            self.send(Cmd::TakeStop);
            self.log(Cat::Sess, "■ session recording stopped (finishing file…)");
            self.finished_takes.push(w);
            return;
        }
        let (tx, rx) = rtrb::RingBuffer::new(self.sr as usize * 4);
        match TakeWriter::start(&self.session, self.sr, rx, self.sys_tx.clone()) {
            Ok(w) => {
                self.send(Cmd::TakeStart(Box::new(tx)));
                self.log(
                    Cat::Sess,
                    format!("● recording session → {}", w.path.display()),
                );
                self.take = Some(w);
            }
            Err(e) => self.log(Cat::Err, format!("cannot record session: {e}")),
        }
    }

    /// Hand over take writers so `main` can wait for them after audio stops.
    pub fn into_takes(mut self) -> Vec<TakeWriter> {
        let mut v = std::mem::take(&mut self.finished_takes);
        v.extend(self.take.take());
        v
    }

    // ------------------------------------------------------------------- keys

    fn on_key(&mut self, k: KeyEvent) {
        dlog!(
            "key {:?} {:?} mods={:?} state={:?} enhanced={} mode={:?} module={:?}",
            k.code,
            k.kind,
            k.modifiers,
            k.state,
            self.enhanced,
            self.mode,
            self.module
        );
        if self.prompt.is_some() {
            if k.kind != KeyEventKind::Release {
                self.prompt_key(k);
            }
            return;
        }
        let press = k.kind == KeyEventKind::Press;
        // A pending delete is confirmed by Delete; any other key cancels it,
        // and Esc does nothing else (no panic).
        if press
            && k.code != KeyCode::Delete
            && let Some(id) = self.pending_delete.take()
        {
            let label = self.label_of(Target::Track(id));
            self.log(Cat::Track, format!("delete of {label} cancelled"));
            self.refresh_footer();
            if k.code == KeyCode::Esc {
                return;
            }
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            if press && let KeyCode::Char(c) = k.code {
                match c.to_ascii_lowercase() {
                    'c' | 'q' => self.quit = true,
                    's' => self.save(),
                    'r' => self.toggle_take(),
                    'z' => self.undo(),
                    _ => {}
                }
            }
            return;
        }
        match k.code {
            KeyCode::Tab if press => self.toggle_mode(),
            KeyCode::F(1) if press => self.set_module(Module::Synth),
            KeyCode::F(2) if press => self.set_module(Module::Voice),
            KeyCode::F(3) if press => self.set_module(Module::Drums),
            KeyCode::F(4) if press => {
                let v = 1.0 - self.live_voice[p::MONITOR].round();
                let _ = self.set_param(Target::LiveVoice, p::MONITOR, v);
            }
            KeyCode::F(5) if press => crate::commands::print_help(self, None),
            KeyCode::Delete if press && self.mode == Mode::Piano => self.log(
                Cat::Track,
                "to delete a track: Tab (control mode), pick it with 1-9, then Delete twice",
            ),
            KeyCode::Char(' ') if press => self.loop_toggle(),
            KeyCode::Backspace if press => self.cancel_recording(),
            KeyCode::Enter if press => {
                self.release_all();
                self.prompt = Some(String::new());
                self.hist_pos = None;
                self.refresh_footer();
            }
            KeyCode::Esc if press => {
                self.release_all();
                self.send(Cmd::Panic);
                self.log(Cat::Sys, "panic: all notes off");
            }
            _ => match self.mode {
                Mode::Piano => self.piano_key(k),
                Mode::Control => self.control_key(k),
            },
        }
    }

    fn prompt_key(&mut self, k: KeyEvent) {
        let Some(text) = self.prompt.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Enter => {
                let line = std::mem::take(text);
                self.prompt = None;
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                    crate::commands::run(self, &line);
                }
            }
            KeyCode::Esc => self.prompt = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.prompt = None,
            KeyCode::Char(c) => text.push(c),
            KeyCode::Up | KeyCode::Down if !self.history.is_empty() => {
                let last = self.history.len() - 1;
                let pos = match (self.hist_pos, k.code) {
                    (None, KeyCode::Up) => Some(last),
                    (Some(i), KeyCode::Up) => Some(i.saturating_sub(1)),
                    (Some(i), KeyCode::Down) if i < last => Some(i + 1),
                    _ => None,
                };
                self.hist_pos = pos;
                *text = pos.map(|i| self.history[i].clone()).unwrap_or_default();
            }
            _ => {}
        }
        self.refresh_footer();
    }

    fn toggle_mode(&mut self) {
        self.release_all();
        self.mode = match self.mode {
            Mode::Piano => Mode::Control,
            Mode::Control => Mode::Piano,
        };
        match self.mode {
            Mode::Piano => self.log(Cat::Sys, "mode → PIANO (keys play notes)"),
            Mode::Control => {
                let label = self.label_of(self.sel_target());
                self.log(Cat::Sys, format!("mode → CONTROL (keys tweak {label})"));
            }
        }
        self.refresh_footer();
    }

    pub fn set_module(&mut self, m: Module) {
        if self.module == m {
            return;
        }
        self.release_all();
        self.module = m;
        if self.sel == Sel::Live {
            self.page = 0;
            self.cursor = 0;
        }
        match m {
            Module::Synth => self.log(Cat::Synth, "module → SYNTH"),
            Module::Voice => {
                let mon = params::format(p::MONITOR, self.live_voice[p::MONITOR]);
                let hint = if self.mic_ok {
                    format!("monitor {mon}, F4 toggles")
                } else {
                    "no microphone!".into()
                };
                self.log(
                    Cat::Voice,
                    format!("module → VOICE ({hint}; piano keys pitch-shift your voice)"),
                );
            }
            Module::Drums => {
                let lock = if self.loop_len.is_some() {
                    ", set by the loop"
                } else {
                    ""
                };
                self.log(
                    Cat::Drum,
                    format!(
                        "module → DRUMS (b kick · s snare · h hihat · c cymbal · {:.1} bpm{lock})",
                        self.bpm
                    ),
                );
            }
        }
    }

    fn undo(&mut self) {
        match self.tracks.last().map(|t| t.id) {
            Some(id) => {
                let n = self.tracks.len();
                if let Some(t) = self.remove_track(id) {
                    self.log(Cat::Track, format!("undo: deleted track {n} '{}'", t.name));
                }
            }
            None => self.log(Cat::Track, "nothing to undo"),
        }
    }

    // ---------------------------------------------------------------- piano

    fn piano_key(&mut self, k: KeyEvent) {
        if self.module == Module::Drums {
            self.drums_key(k);
            return;
        }
        let press = k.kind == KeyEventKind::Press;
        let pressish = press || k.kind == KeyEventKind::Repeat;
        match k.code {
            KeyCode::Left | KeyCode::Right if pressish => {
                self.release_all();
                let d = if k.code == KeyCode::Left { -1 } else { 1 };
                self.octave = (self.octave + d).clamp(0, 7);
                let lo = keymap::note_name(((self.octave + 1) * 12) as u8);
                self.log(
                    Cat::Sys,
                    format!("octave {} (lower row starts at {lo})", self.octave),
                );
            }
            KeyCode::Up | KeyCode::Down if pressish => {
                let d: i32 = if k.code == KeyCode::Up { 8 } else { -8 };
                self.velocity = (self.velocity as i32 + d).clamp(1, 127) as u8;
                self.log(Cat::Sys, format!("velocity {}", self.velocity));
            }
            KeyCode::Char(c) => {
                let c = keymap::base_char(c);
                let Some(off) = keymap::piano_offset(c) else {
                    return;
                };
                let note = ((self.octave + 1) * 12 + off).clamp(0, 127) as u8;
                match k.kind {
                    KeyEventKind::Press => {
                        if self.held.contains_key(&c) {
                            // Legacy terminals send repeats as presses, and only for the
                            // most recently pressed key. Someone is still holding keys, so
                            // keep every sounding note alive: this is what makes chords
                            // work without key-release events.
                            if !self.enhanced {
                                let until = Instant::now() + Duration::from_secs_f32(self.gate);
                                for h in self.held.values_mut() {
                                    h.until = Some(until);
                                }
                            }
                            return;
                        }
                        self.note_down(c, note);
                    }
                    KeyEventKind::Release => self.note_up(c),
                    KeyEventKind::Repeat => {}
                }
            }
            _ => {}
        }
    }

    fn note_down(&mut self, c: char, note: u8) {
        let until = (!self.enhanced).then(|| Instant::now() + Duration::from_secs_f32(self.gate));
        self.held.insert(
            c,
            Held {
                note,
                module: self.module,
                until,
            },
        );
        match self.module {
            Module::Synth => {
                self.send(Cmd::NoteOn(note, self.velocity));
                let rec = if matches!(self.rec, RecUi::Recording(Kind::Synth, _)) {
                    "  ●rec"
                } else {
                    ""
                };
                self.log(
                    Cat::Synth,
                    format!(
                        "♪ {:<4} ({note:>3})  vel {}{rec}",
                        keymap::note_name(note),
                        self.velocity
                    ),
                );
            }
            Module::Voice => {
                let semis = (note as f32 - 60.0).clamp(-24.0, 24.0);
                self.voice_keys.push((c, semis));
                self.send(Cmd::KeyPitch(semis));
                self.log(
                    Cat::Voice,
                    format!("voice pitch {semis:+.0} st ({})", keymap::note_name(note)),
                );
            }
            Module::Drums => {}
        }
    }

    fn note_up(&mut self, c: char) {
        let Some(h) = self.held.remove(&c) else {
            return;
        };
        match h.module {
            Module::Synth => {
                self.send(Cmd::NoteOff(h.note));
                if self.verbose {
                    self.log(Cat::Synth, format!("  {} off", keymap::note_name(h.note)));
                }
            }
            Module::Voice => {
                self.voice_keys.retain(|(k, _)| *k != c);
                let semis = self.voice_keys.last().map(|(_, s)| *s).unwrap_or(0.0);
                self.send(Cmd::KeyPitch(semis));
                if self.voice_keys.is_empty() && self.verbose {
                    self.log(Cat::Voice, "voice pitch back to normal");
                }
            }
            Module::Drums => {}
        }
    }

    pub fn release_all(&mut self) {
        let keys: Vec<char> = self.held.keys().copied().collect();
        for c in keys {
            self.note_up(c);
        }
    }

    // ---------------------------------------------------------------- drums

    fn drums_key(&mut self, k: KeyEvent) {
        let press = k.kind == KeyEventKind::Press;
        let pressish = press || k.kind == KeyEventKind::Repeat;
        match k.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right if pressish => {
                let d = match k.code {
                    KeyCode::Up => 1.0,
                    KeyCode::Down => -1.0,
                    KeyCode::Right => 10.0,
                    _ => -10.0,
                };
                if let Err(e) = self.set_bpm(self.bpm.round() + d) {
                    self.log(Cat::Warn, e);
                }
            }
            KeyCode::Char(raw) if press => {
                let c = keymap::base_char(raw);
                // Legacy terminals report a held key as a stream of presses.
                let now = Instant::now();
                let repeat = !self.enhanced
                    && self
                        .last_drum_key
                        .is_some_and(|(lc, t)| lc == c && now - t < Duration::from_millis(300));
                self.last_drum_key = Some((c, now));
                if repeat {
                    return;
                }
                let new = raw.is_ascii_uppercase() || k.modifiers.contains(KeyModifiers::SHIFT);
                match c {
                    'b' => self.drum_key(drums::KICK, new),
                    's' => self.drum_key(drums::SNARE, new),
                    'h' => self.drum_key(drums::HIHAT, new),
                    'c' => self.drum_key(drums::CYMBAL, new),
                    't' => self.tap_tempo(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// The newest drum track playing instrument `inst`.
    fn drum_track(&self, inst: u8) -> Option<u32> {
        self.tracks
            .iter()
            .rev()
            .find(|t| t.kind == Kind::Drum && t.params[p::DRUM] as u8 == inst)
            .map(|t| t.id)
    }

    /// Drum key: start the instrument, or mute/unmute it if it already runs.
    /// `new` always adds another track (Shift).
    pub fn drum_key(&mut self, inst: u8, new: bool) {
        match self.drum_track(inst) {
            Some(id) if !new => {
                crate::commands::set_mute(self, id, None);
                if self.sel != Sel::Track(id) {
                    self.sel = Sel::Track(id);
                    self.page = 0;
                    self.cursor = 0;
                }
            }
            _ => self.add_drum(inst),
        }
        self.refresh_footer();
    }

    fn add_drum(&mut self, inst: u8) {
        if self.tracks.len() >= crate::engine::MAX_TRACKS {
            self.log(Cat::Warn, "track limit reached; delete a track first");
            return;
        }
        let sr = self.sr as f64;
        let bar = 240.0 * sr / self.bpm as f64;
        let (len, bars) = match self.loop_len {
            None => {
                let len = bar.round() as u64;
                self.loop_len = Some(len);
                self.log(
                    Cat::Loop,
                    format!(
                        "loop length set to 1 bar at {:.1} bpm ({:.2} s)",
                        self.bpm,
                        self.secs(len)
                    ),
                );
                (len, 1)
            }
            Some(l) => {
                // Fit a whole number of bars into the existing loop.
                let bars = ((l as f64 / bar).round() as u32).max(1);
                let fitted = (240.0 * sr * bars as f64 / l as f64) as f32;
                if (fitted - self.bpm).abs() > 0.05 {
                    self.log(
                        Cat::Drum,
                        format!(
                            "tempo fitted to the {:.2} s loop: {bars} bar{} at {fitted:.1} bpm",
                            self.secs(l),
                            if bars == 1 { "" } else { "s" }
                        ),
                    );
                }
                self.bpm = fitted;
                (l, bars)
            }
        };
        let mut params = self.live_drums;
        let (rate, offset) = drums::DEFAULT_PATTERN[inst as usize];
        params[p::DRUM] = inst as f32;
        params[p::RATE] = rate;
        params[p::OFFSET] = offset;
        // With no other track the loop starts on this very hit.
        let now = self.tracks.is_empty() || self.phase_pending;
        if self.tracks.is_empty() {
            self.phase_pending = true;
        }
        let id = self.next_id;
        let name = format!("{}-{id}", drums::name(inst));
        let model = TrackModel {
            id,
            name: name.clone(),
            kind: Kind::Drum,
            params,
            len,
            t0: self.loop_t0,
            muted: false,
            solo: false,
            data: TrackData::Drum { bars },
        };
        self.push_track(model, now);
        self.sel = Sel::Track(id);
        self.page = 0;
        self.cursor = 0;
        let n = self.tracks.len();
        self.log(
            Cat::Drum,
            format!(
                "▶ track {n} '{name}' · {} · {:.1} bpm · looping (press again to mute, Tab to tweak)",
                params::format(p::RATE, rate),
                self.bpm
            ),
        );
    }

    pub fn set_bpm(&mut self, bpm: f32) -> Result<(), String> {
        if let Some(l) = self.loop_len {
            return Err(format!(
                "tempo is locked to the {:.2} s loop ({:.1} bpm) — 'clear all' to start over",
                self.secs(l),
                self.bpm
            ));
        }
        let bpm = (bpm.clamp(BPM_MIN, BPM_MAX) * 10.0).round() / 10.0;
        self.bpm = bpm;
        self.log(Cat::Drum, format!("tempo {bpm:.1} bpm"));
        Ok(())
    }

    fn tap_tempo(&mut self) {
        let now = Instant::now();
        if self
            .taps
            .last()
            .is_some_and(|t| now - *t > Duration::from_secs(2))
        {
            self.taps.clear();
        }
        self.taps.push(now);
        if self.taps.len() > 5 {
            self.taps.remove(0);
        }
        let n = self.taps.len();
        if n < 2 {
            self.log(Cat::Drum, "tap… (keep tapping t on the beat)");
            return;
        }
        let beat = (self.taps[n - 1] - self.taps[0]).as_secs_f32() / (n - 1) as f32;
        if let Err(e) = self.set_bpm(60.0 / beat) {
            self.log(Cat::Warn, e);
        }
    }

    fn on_midi(&mut self, m: MidiLog) {
        let rec = if matches!(self.rec, RecUi::Recording(Kind::Synth, _)) {
            "  ●rec"
        } else {
            ""
        };
        match m {
            MidiLog::NoteOn(n, v) => {
                self.log(
                    Cat::Midi,
                    format!("♪ {:<4} ({n:>3})  vel {v}{rec}", keymap::note_name(n)),
                );
            }
            MidiLog::NoteOff(n) if self.verbose => {
                self.log(Cat::Midi, format!("  {} off", keymap::note_name(n)));
            }
            MidiLog::Sustain(down) => {
                self.log(
                    Cat::Midi,
                    format!("sustain pedal {}", if down { "down" } else { "up" }),
                );
            }
            MidiLog::Mod(v) if self.verbose => self.log(Cat::Midi, format!("mod wheel {v}")),
            _ => {}
        }
    }

    // -------------------------------------------------------------- control

    pub fn pages(&self) -> Vec<params::Page> {
        self.scope_of(self.sel_target())
            .map(|s| s.pages())
            .unwrap_or_default()
    }

    pub fn select(&mut self, sel: Sel) {
        self.sel = sel;
        self.page = 0;
        self.cursor = 0;
        let label = self.label_of(self.sel_target());
        self.log(Cat::Ctrl, format!("control target → {label}"));
        self.refresh_footer();
    }

    fn nudge(&mut self, idx: usize, dir: i32, coarse: bool) {
        let t = self.sel_target();
        if let Some(v) = self.params_of(t).map(|prm| prm[idx]) {
            let nv = params::nudge(idx, v, dir, coarse);
            if nv != v {
                let _ = self.set_param(t, idx, nv);
            }
        }
    }

    fn control_key(&mut self, k: KeyEvent) {
        let press = k.kind == KeyEventKind::Press;
        let pressish = press || k.kind == KeyEventKind::Repeat;
        if !pressish {
            return;
        }
        if k.code == KeyCode::Delete {
            if press {
                self.delete_key();
                self.refresh_footer();
            }
            return;
        }
        let pages = self.pages();
        if pages.is_empty() {
            return;
        }
        self.page = self.page.min(pages.len() - 1);
        let page = &pages[self.page];
        self.cursor = self.cursor.min(page.params.len().saturating_sub(1));
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Up => self.cursor = (self.cursor + page.params.len() - 1) % page.params.len(),
            KeyCode::Down => self.cursor = (self.cursor + 1) % page.params.len(),
            KeyCode::Left | KeyCode::Right => {
                let dir = if k.code == KeyCode::Left { -1 } else { 1 };
                if let Some(&idx) = page.params.get(self.cursor) {
                    self.nudge(idx, dir, shift);
                }
            }
            KeyCode::Char(raw) => {
                let coarse = shift || raw.is_ascii_uppercase() || keymap::base_char(raw) != raw;
                let c = keymap::base_char(raw);
                if let Some((slot, dir)) = keymap::slot_key(c) {
                    if let Some(&idx) = page.params.get(slot) {
                        self.cursor = slot;
                        self.nudge(idx, dir, coarse);
                    }
                    return;
                }
                if !press {
                    return;
                }
                match c {
                    '[' | ']' => {
                        let n = pages.len();
                        self.page = if c == '[' {
                            (self.page + n - 1) % n
                        } else {
                            (self.page + 1) % n
                        };
                        self.cursor = 0;
                        let pg = &self.pages()[self.page];
                        self.log(
                            Cat::Ctrl,
                            format!("page {}/{n}: {}", self.page + 1, pg.title),
                        );
                    }
                    '`' => self.select(Sel::Live),
                    '0' => self.select(Sel::Master),
                    '1'..='9' => {
                        let i = c as usize - '1' as usize;
                        match self.tracks.get(i) {
                            Some(t) => self.select(Sel::Track(t.id)),
                            None => self.log(Cat::Warn, format!("there is no track {}", i + 1)),
                        }
                    }
                    ',' | '.' => self.cycle_target(if c == ',' { -1 } else { 1 }),
                    'z' => {
                        if let Some(&idx) = page.params.get(self.cursor) {
                            let _ =
                                self.set_param(self.sel_target(), idx, params::SPECS[idx].default);
                        }
                    }
                    'x' => crate::commands::show_params(self, self.sel_target()),
                    'c' => {
                        let t = self.sel_target();
                        if let (Some(prm), Some(scope)) =
                            (self.params_of(t).copied(), self.scope_of(t))
                        {
                            self.clipboard = Some((prm, scope));
                            let label = self.label_of(t);
                            self.log(Cat::Ctrl, format!("copied sound of {label}"));
                        }
                    }
                    'v' => {
                        let t = self.sel_target();
                        match self.clipboard {
                            Some((prm, _)) => {
                                let _ = self.paste_params(t, &prm);
                            }
                            None => {
                                self.log(Cat::Warn, "clipboard is empty (press c to copy a sound)")
                            }
                        }
                    }
                    'm' | 'n' => {
                        if let Sel::Track(id) = self.sel {
                            if c == 'm' {
                                crate::commands::set_mute(self, id, None);
                            } else {
                                crate::commands::set_solo(self, id, None);
                            }
                        } else {
                            self.log(Cat::Warn, "select a track (1-9) to mute/solo it");
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        self.refresh_footer();
    }

    /// Delete pressed in control mode: arm, or confirm when already armed.
    fn delete_key(&mut self) {
        let Sel::Track(id) = self.sel else {
            self.log(
                Cat::Warn,
                "select a track (1-9) first, then press Delete twice to delete it",
            );
            return;
        };
        let Some(i) = self.track_index(id) else {
            return;
        };
        if self.pending_delete == Some(id) {
            if let Some(t) = self.remove_track(id) {
                self.log(
                    Cat::Track,
                    format!("✖ deleted track {} '{}'", i + 1, t.name),
                );
                if self.tracks.is_empty() && self.loop_len.is_some() {
                    self.log(
                        Cat::Track,
                        "no tracks left (loop length kept — 'loop clear' resets it)",
                    );
                }
            }
        } else {
            self.pending_delete = Some(id);
            let name = self.tracks[i].name.clone();
            self.log(
                Cat::Warn,
                format!(
                    "delete track {} '{name}'? press Delete again to confirm · Esc to cancel",
                    i + 1
                ),
            );
        }
    }

    /// Copy every applicable parameter from `src` onto target `t`.
    pub fn paste_params(&mut self, t: Target, src: &Params) -> Result<usize, String> {
        let scope = self.scope_of(t).ok_or("no such target")?;
        let dst = self.params_mut(t).ok_or("no such target")?;
        let mut n = 0;
        for i in 0..params::NUM {
            if scope.applies(i) && dst[i] != src[i] {
                dst[i] = src[i];
                n += 1;
            }
        }
        self.sync_params(t);
        let label = self.label_of(t);
        self.log(
            Cat::Ctrl,
            format!("applied sound to {label} ({n} parameters changed)"),
        );
        Ok(n)
    }

    fn cycle_target(&mut self, dir: i32) {
        let mut all = vec![Sel::Live];
        all.extend(self.tracks.iter().map(|t| Sel::Track(t.id)));
        all.push(Sel::Master);
        let cur = all.iter().position(|s| *s == self.sel).unwrap_or(0) as i32;
        let n = all.len() as i32;
        self.select(all[(cur + dir).rem_euclid(n) as usize]);
    }

    // --------------------------------------------------------------- footer

    pub fn refresh_footer(&mut self) {
        self.last_footer = Instant::now();
        let lines = self.footer_lines();
        self.meter_out = 0.0;
        self.meter_mic = 0.0;
        self.term.set_footer(lines);
    }

    fn footer_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let module = match self.module {
            Module::Synth => "SYNTH",
            Module::Voice => "VOICE",
            Module::Drums => "DRUMS",
        };
        let mode = match self.mode {
            Mode::Piano => "PIANO",
            Mode::Control => "CTRL",
        };
        let transport = match self.loop_len {
            Some(l) => {
                let phase = ((self.clock + l - self.loop_t0 % l) % l) as f32 / l as f32;
                let filled = (phase * 8.0) as usize;
                format!(
                    "loop {:.2}s {}{}",
                    self.secs(l),
                    "▰".repeat(filled),
                    "▱".repeat(8 - filled)
                )
            }
            None => "no loop yet".to_string(),
        };
        let rec = match self.rec {
            RecUi::Idle => String::new(),
            RecUi::Armed(_) => " │ ◌ ARMED: play to start".to_string(),
            RecUi::Recording(k, t0) => {
                format!(
                    " │ ● REC {} {:.1}s",
                    k.label(),
                    self.secs(self.clock.saturating_sub(t0))
                )
            }
        };
        let take = if self.take.is_some() {
            " │ ● TAKE"
        } else {
            ""
        };
        let midi = self.midi.port.as_deref().map(short).unwrap_or("–".into());
        let mic = if self.mic_ok {
            meter(self.meter_mic)
        } else {
            "off".into()
        };
        lines.push(format!(
            " {module}·{mode} │ {transport}{rec}{take} │ {} trk │ out {} │ mic {mic} │ midi {midi} ",
            self.tracks.len(),
            meter(self.meter_out),
        ));

        match self.mode {
            Mode::Piano if self.module == Module::Drums => {
                let lock = if self.loop_len.is_some() {
                    " (locked to loop)"
                } else {
                    " ↑↓ ±1 ←→ ±10 t tap"
                };
                let state = |inst: u8| {
                    let (mut any, mut playing) = (false, false);
                    for t in self
                        .tracks
                        .iter()
                        .filter(|t| t.kind == Kind::Drum && t.params[p::DRUM] as u8 == inst)
                    {
                        any = true;
                        playing |= !t.muted;
                    }
                    match (any, playing) {
                        (false, _) => "·",
                        (true, true) => "▶",
                        (true, false) => "■",
                    }
                };
                lines.push(format!(
                    " {:.1} bpm{lock} │ b kick {} · s snare {} · h hihat {} · c cymbal {} │ key = start/mute · Shift+key = another · Tab control · F1 synth F2 voice · F5 help ",
                    self.bpm,
                    state(drums::KICK),
                    state(drums::SNARE),
                    state(drums::HIHAT),
                    state(drums::CYMBAL),
                ));
            }
            Mode::Piano => {
                let first = match self.module {
                    Module::Synth => format!(
                        " oct {} (z={}, q={}) vel {} voices {}",
                        self.octave,
                        keymap::note_name(((self.octave + 1) * 12) as u8),
                        keymap::note_name(((self.octave + 2) * 12) as u8),
                        self.velocity,
                        self.voices
                    ),
                    Module::Voice => format!(
                        " keys pitch-shift your voice │ monitor {} │ pitch {}",
                        params::format(p::MONITOR, self.live_voice[p::MONITOR]),
                        params::format(p::PITCH, self.live_voice[p::PITCH]),
                    ),
                    Module::Drums => String::new(),
                };
                lines.push(format!(
                    "{first} │ Space loop · ⌫ cancel · Tab control · F1 synth F2 voice F3 drums · Enter command · F5 help "
                ));
            }
            Mode::Control => {
                let t = self.sel_target();
                let pages = self.pages();
                let page_i = self.page.min(pages.len().saturating_sub(1));
                lines.push(format!(
                    " target {} │ page {}/{} {} │ ↑↓ select ←→ adjust · [ ] page · ` live · 1-9 track · 0 master · x show · Del delete ",
                    self.label_of(t),
                    page_i + 1,
                    pages.len(),
                    pages.get(page_i).map(|p| p.title).unwrap_or(""),
                ));
                if let (Some(page), Some(prm)) = (pages.get(page_i), self.params_of(t)) {
                    let cell_w = 27;
                    let per_row = (self.term.width() / cell_w).clamp(1, 5);
                    let cells: Vec<String> = page
                        .params
                        .iter()
                        .enumerate()
                        .map(|(slot, &idx)| {
                            let mark = if slot == self.cursor { '›' } else { ' ' };
                            let cell = format!(
                                "{mark}{}/{} {} {}",
                                keymap::UP_KEYS[slot],
                                keymap::DOWN_KEYS[slot],
                                params::SPECS[idx].name,
                                params::format(idx, prm[idx])
                            );
                            format!("{cell:<cell_w$}")
                        })
                        .collect();
                    for row in cells.chunks(per_row) {
                        lines.push(format!(" {}", row.concat()));
                    }
                }
                if let Some(id) = self.pending_delete {
                    lines.push(format!(
                        " ⚠ DELETE {}? press Delete again to confirm · Esc to cancel ",
                        self.label_of(Target::Track(id))
                    ));
                }
            }
        }
        if let Some(text) = &self.prompt {
            lines.push(format!(
                " : {text}▏  (Enter run · Esc cancel · ↑ history · 'help')"
            ));
        }
        lines
    }
}

fn short(name: &str) -> String {
    let n: String = name.chars().take(24).collect();
    if name.chars().count() > 24 {
        format!("{n}…")
    } else {
        n
    }
}

/// 6-segment peak meter on a dB scale (-48..0 dBFS).
fn meter(peak: f32) -> String {
    let db = 20.0 * peak.max(1e-6).log10();
    let lit = (((db + 48.0) / 48.0) * 6.0).clamp(0.0, 6.0).round() as usize;
    let clip = if peak >= 0.99 { "!" } else { "" };
    format!("{}{}{clip}", "▮".repeat(lit), "·".repeat(6 - lit))
}

/// Turn the raw events of a synth recording into a sorted loop sequence.
pub fn fold_events(events: Vec<SeqEvent>, frames: u64, len: u64) -> Vec<SeqEvent> {
    let mut on = [false; 128];
    let mut sustain = false;
    let mut out = events;
    for e in &out {
        match e.ev {
            Ev::On { note, .. } => on[note as usize & 127] = true,
            Ev::Off { note } => on[note as usize & 127] = false,
            Ev::Sustain { down } => sustain = down,
        }
    }
    // Close anything still held when recording stopped, just before the end.
    let end = frames.saturating_sub(1);
    for (note, held) in on.iter().enumerate() {
        if *held {
            out.push(SeqEvent {
                at: end,
                ev: Ev::Off { note: note as u8 },
            });
        }
    }
    if sustain {
        out.push(SeqEvent {
            at: end,
            ev: Ev::Sustain { down: false },
        });
    }
    for e in &mut out {
        e.at %= len;
    }
    // At equal positions: releases before presses so wrapped notes retrigger.
    out.sort_by_key(SeqEvent::order_key);
    out
}

/// Fade the edges and wrap a voice recording onto a loop of `len` frames.
pub fn fold_audio(audio: &[f32], len: usize, sr: u32) -> Vec<f32> {
    let fade = ((sr as usize) / 200).min(audio.len() / 2).max(1);
    let mut out = vec![0.0; len];
    let n = audio.len();
    for (i, s) in audio.iter().enumerate() {
        let g = if i < fade {
            i as f32 / fade as f32
        } else if i >= n - fade {
            (n - 1 - i) as f32 / fade as f32
        } else {
            1.0
        };
        out[i % len] += s * g;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_notes_are_closed_and_wrapped() {
        let ev = vec![
            SeqEvent {
                at: 0,
                ev: Ev::On { note: 60, vel: 100 },
            },
            SeqEvent {
                at: 1200,
                ev: Ev::On { note: 64, vel: 100 },
            },
        ];
        let out = fold_events(ev, 1500, 1000);
        // note 64 starts at 1200 -> wraps to 200; both get an Off at 1499 -> 499.
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].at, 0);
        assert_eq!(
            out[1],
            SeqEvent {
                at: 200,
                ev: Ev::On { note: 64, vel: 100 }
            }
        );
        assert!(
            out[2..]
                .iter()
                .all(|e| e.at == 499 && matches!(e.ev, Ev::Off { .. }))
        );
    }

    #[test]
    fn release_sorts_before_press_at_same_time() {
        let ev = vec![
            SeqEvent {
                at: 0,
                ev: Ev::On { note: 60, vel: 100 },
            },
            SeqEvent {
                at: 1000,
                ev: Ev::Off { note: 60 },
            },
        ];
        let out = fold_events(ev, 1001, 1000);
        assert!(matches!(out[0].ev, Ev::Off { .. }));
        assert!(matches!(out[1].ev, Ev::On { .. }));
    }

    #[test]
    fn audio_folds_overflow_onto_start() {
        let audio = vec![1.0; 1500];
        let out = fold_audio(&audio, 1000, 48000);
        assert_eq!(out.len(), 1000);
        // 1250 -> 250 overlaps at full gain (both ends' 240-sample fades avoided);
        // 1400 -> 400 is inside the fade-out.
        assert!((out[250] - 2.0).abs() < 1e-6);
        assert!(out[400] > 1.0 && out[400] < 2.0);
        assert!((out[700] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter(0.0), "······");
        assert_eq!(meter(1.0), "▮▮▮▮▮▮!");
    }
}
