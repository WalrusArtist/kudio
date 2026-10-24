//! The command prompt (opened with Enter). Commands address tracks by their
//! 1-based index as shown by `tracks`, or `live`, `synth`, `voice`, `master`.

use crate::app::{App, BPM_MAX, BPM_MIN, Mode, Module, Sel};
use crate::engine::{Cmd, Target};
use crate::params::{self, Scope};
use crate::session::{self, TrackModel};
use crate::term::Cat;

pub const KEY_HELP: &str = "\
GLOBAL KEYS (piano + control mode)
  Tab        toggle PIANO / CONTROL mode       F1 synth · F2 voice · F3 drums module
  Space      loop: start recording · press again to close the loop (→ new track)
  Backspace  cancel the recording in progress  F4 mic monitoring on/off (headphones!)
  Enter      open the command prompt           Esc panic (all notes off)
  Ctrl+S save session · Ctrl+R record session to WAV · Ctrl+Z delete last track · Ctrl+Q quit
PIANO MODE (synth / voice)
  z s x d c v g b h n j m , l . ; /        lower octave (white + black keys)
  q 2 w 3 e r 5 t 6 y 7 u i 9 o 0 p [ = ]  upper octave
  ← → octave · ↑ ↓ velocity · in the VOICE module keys pitch-shift your voice (C4 = 0)
PIANO MODE (drums, F3)
  b kick · s snare · h hihat · c cymbal   first press starts a looping drum track, next press mutes/unmutes
  Shift+b/s/h/c  add another track of that drum · ↑ ↓ bpm ±1 · ← → bpm ±10 · t tap tempo
CONTROL MODE
  q w e r t y u i o p  raise parameter 1-10 of the page · a s d f g h j k l ; lower it
  Shift = bigger steps · ↑ ↓ select · ← → adjust · [ ] page
  ` live sound · 1-9 track · 0 master · , . previous/next target
  z reset param · x show all · c copy sound · v paste sound · m mute · n solo
  Delete twice   delete the selected track (Esc cancels)";

pub const CMD_HELP: &str = "\
COMMANDS   (targets: track number, live, synth, voice, drums, master)
  tracks                         list loop tracks
  show <t>                       all parameters of a target
  set <t> <param> <value> ...    e.g. set 2 cutoff 800 reso 0.6  ·  shorthand: 2 dmix 30%
                                 values: 800, 2k, 50%, 20ms, saw, 1/8, on/off, *2, +=3, -=3
  reset <t> [param ...]          back to defaults
  sel <t>                        point control mode at a target
  mute | unmute | solo | unsolo <n ...>
  del <n> · name <n> <name> · dup <n> · copy <from> <to>
  bpm [n]                        show or set the drum tempo (before the first loop)
  drums [kick|snare|hihat|cymbal]  drums module · with a name: add a drum track
  preset save|load <name> [t] · presets
  save [name] · load <name> · sessions
  rec                            start/stop recording the session to a WAV take
  monitor on|off · midi [n|name|off] · gate <s> · verbose · octave <n> · vel <n>
  loop [clear | <seconds>] · clear all · synth · voice · quit";

pub fn run(app: &mut App, line: &str) {
    app.log(Cat::Cmd, format!("› {}", line.trim()));
    if let Err(e) = exec(app, line.trim()) {
        app.log(Cat::Err, e);
    }
    app.refresh_footer();
}

pub fn print_help(app: &mut App, topic: Option<&str>) {
    match topic {
        Some(t) if t.starts_with("key") => app.log(Cat::Help, KEY_HELP),
        Some(t) if t.starts_with("com") || t.starts_with("cmd") => app.log(Cat::Help, CMD_HELP),
        Some(t) => match params::find(t) {
            Some(i) => {
                let s = &params::SPECS[i];
                app.log(
                    Cat::Help,
                    format!(
                        "{}: {} (range {} … {}, default {})",
                        s.name,
                        s.help,
                        params::format(i, s.min),
                        params::format(i, s.max),
                        params::format(i, s.default)
                    ),
                )
            }
            None => app.log(
                Cat::Help,
                format!("no help for '{t}' — try 'help keys' or 'help commands'"),
            ),
        },
        None => {
            app.log(Cat::Help, KEY_HELP);
            app.log(Cat::Help, CMD_HELP);
        }
    }
}

fn parse_target(app: &App, s: &str) -> Option<Target> {
    match s.to_ascii_lowercase().as_str() {
        "live" | "l" => Some(app.live_target()),
        "synth" => Some(Target::LiveSynth),
        "voice" | "mic" => Some(Target::LiveVoice),
        "drums" | "drum" => Some(Target::LiveDrums),
        "master" => Some(Target::Master),
        "sel" | "." => Some(app.sel_target()),
        n => {
            let i: usize = n.trim_start_matches('#').parse().ok()?;
            app.tracks
                .get(i.checked_sub(1)?)
                .map(|t| Target::Track(t.id))
        }
    }
}

fn target_arg(app: &App, s: Option<&&str>) -> Result<Target, String> {
    match s {
        None => Ok(app.sel_target()),
        Some(s) => parse_target(app, s).ok_or_else(|| bad_target(app, s)),
    }
}

fn bad_target(app: &App, s: &str) -> String {
    if s.parse::<usize>().is_ok() {
        format!(
            "there is no track {s} (there are {} tracks)",
            app.tracks.len()
        )
    } else {
        format!("unknown target '{s}' — use a track number, live, synth, voice, drums or master")
    }
}

fn track_id(app: &App, s: &str) -> Result<u32, String> {
    match parse_target(app, s) {
        Some(Target::Track(id)) => Ok(id),
        _ => Err(bad_target(app, s)),
    }
}

fn exec(app: &mut App, line: &str) -> Result<(), String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let Some(first) = words.first() else {
        return Ok(());
    };
    let cmd = first.to_ascii_lowercase();
    let args = &words[1..];
    if cmd.trim_start_matches('#').parse::<usize>().is_ok() {
        return set(app, &words);
    }
    match cmd.as_str() {
        "help" | "h" | "?" => print_help(app, args.first().copied()),
        "tracks" | "ls" | "list" => list_tracks(app),
        "show" | "params" | "p" => {
            let t = target_arg(app, args.first())?;
            show_params(app, t);
        }
        "set" | "s" => set(app, args)?,
        "reset" => reset(app, args)?,
        "sel" | "select" | "edit" => {
            let t = target_arg(app, args.first())?;
            match t {
                Target::LiveSynth => {
                    app.set_module(Module::Synth);
                    app.select(Sel::Live);
                }
                Target::LiveVoice => {
                    app.set_module(Module::Voice);
                    app.select(Sel::Live);
                }
                Target::LiveDrums => {
                    app.set_module(Module::Drums);
                    app.select(Sel::Live);
                }
                Target::Master => app.select(Sel::Master),
                Target::Track(id) => app.select(Sel::Track(id)),
            }
            if app.mode != Mode::Control {
                app.mode = Mode::Control;
                app.log(Cat::Sys, "mode → CONTROL");
            }
        }
        "mute" | "unmute" | "solo" | "unsolo" => {
            if args.is_empty() {
                if cmd == "unsolo" {
                    let ids: Vec<u32> =
                        app.tracks.iter().filter(|t| t.solo).map(|t| t.id).collect();
                    for id in ids {
                        set_solo(app, id, Some(false));
                    }
                    return Ok(());
                }
                return Err(format!("usage: {cmd} <track number ...>"));
            }
            let ids = args
                .iter()
                .map(|a| track_id(app, a))
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                match cmd.as_str() {
                    "mute" => set_mute(app, id, Some(true)),
                    "unmute" => set_mute(app, id, Some(false)),
                    "solo" => set_solo(app, id, Some(true)),
                    _ => set_solo(app, id, Some(false)),
                }
            }
        }
        "del" | "rm" | "delete" => {
            if args.is_empty() {
                return Err("usage: del <track number ...>".into());
            }
            let ids = args
                .iter()
                .map(|a| track_id(app, a))
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                if let Some(t) = app.remove_track(id) {
                    app.log(Cat::Track, format!("deleted '{}'", t.name));
                }
            }
            if app.tracks.is_empty() && app.loop_len.is_some() {
                app.log(
                    Cat::Track,
                    "no tracks left (loop length kept — 'loop clear' resets it)",
                );
            }
        }
        "name" | "rename" => {
            let (Some(n), true) = (args.first(), args.len() >= 2) else {
                return Err("usage: name <track number> <new name>".into());
            };
            let id = track_id(app, n)?;
            let name = args[1..].join("-");
            if let Some(i) = app.track_index(id) {
                let old = std::mem::replace(&mut app.tracks[i].name, name.clone());
                app.log(Cat::Track, format!("track {} '{old}' → '{name}'", i + 1));
            }
        }
        "dup" | "clone" => {
            let id = track_id(app, args.first().ok_or("usage: dup <track number>")?)?;
            let i = app.track_index(id).ok_or("no such track")?;
            let mut m: TrackModel = app.tracks[i].clone();
            m.id = app.next_id;
            m.name = format!("{}-copy", m.name);
            m.solo = false;
            let name = m.name.clone();
            app.add_track(m);
            let n = app.tracks.len();
            app.log(
                Cat::Track,
                format!("duplicated track {} → track {n} '{name}'", i + 1),
            );
        }
        "copy" => {
            let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
                return Err("usage: copy <from> <to>   e.g. copy 2 live".into());
            };
            let from = parse_target(app, a).ok_or_else(|| bad_target(app, a))?;
            let to = parse_target(app, b).ok_or_else(|| bad_target(app, b))?;
            let src = *app.params_of(from).ok_or("no such source")?;
            app.paste_params(to, &src)?;
        }
        "preset" | "presets" => preset(app, args)?,
        "save" => {
            if let Some(name) = args.first() {
                if !session::valid_name(name) {
                    return Err("session names may contain letters, digits, - _ . only".into());
                }
                app.session = name.to_string();
            }
            app.save();
        }
        "load" => {
            let name = args
                .first()
                .ok_or("usage: load <session name>   (see 'sessions')")?;
            if !session::valid_name(name) || !session::exists(name) {
                return Err(format!("no session named '{name}' — see 'sessions'"));
            }
            app.save();
            let loaded = session::load(name, app.sr)?;
            let n = loaded.tracks.len();
            app.apply_loaded(loaded);
            app.session = name.to_string();
            app.log(Cat::Sess, format!("loaded session '{name}' ({n} tracks)"));
        }
        "sessions" => {
            let list = session::list_sessions();
            let msg = if list.is_empty() {
                "no saved sessions yet".to_string()
            } else {
                list.join("  ")
            };
            app.log(
                Cat::Sess,
                format!("sessions in {}:\n{msg}", session::sessions_dir().display()),
            );
        }
        "rec" | "take" => app.toggle_take(),
        "monitor" | "mon" => {
            let cur = app.live_voice[params::p::MONITOR];
            let v = match args.first().map(|s| s.to_ascii_lowercase()) {
                Some(s) if s == "on" => 1.0,
                Some(s) if s == "off" => 0.0,
                None => 1.0 - cur.round(),
                Some(s) => return Err(format!("monitor on|off, not '{s}'")),
            };
            app.set_param(Target::LiveVoice, params::p::MONITOR, v)?;
        }
        "midi" => midi(app, args)?,
        "gate" => {
            let s: f32 = args.first().and_then(|s| s.parse().ok()).ok_or(
                "usage: gate <seconds>  (note length when the terminal cannot report key releases)",
            )?;
            app.gate = s.clamp(0.05, 5.0);
            app.log(Cat::Sys, format!("gate {:.2} s", app.gate));
        }
        "verbose" | "v" => {
            app.verbose = match args.first().copied() {
                Some("on") => true,
                Some("off") => false,
                _ => !app.verbose,
            };
            app.log(
                Cat::Sys,
                format!("verbose logging {}", if app.verbose { "on" } else { "off" }),
            );
        }
        "octave" | "oct" => {
            let o: i32 = args
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or("usage: octave <0-7>")?;
            app.octave = o.clamp(0, 7);
            app.log(Cat::Sys, format!("octave {}", app.octave));
        }
        "vel" | "velocity" => {
            let v: i32 = args
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or("usage: vel <1-127>")?;
            app.velocity = v.clamp(1, 127) as u8;
            app.log(Cat::Sys, format!("velocity {}", app.velocity));
        }
        "loop" => loop_cmd(app, args)?,
        "clear" => {
            if args.first() != Some(&"all") {
                return Err(
                    "type 'clear all' to delete every track and reset the loop length".into(),
                );
            }
            let ids: Vec<u32> = app.tracks.iter().map(|t| t.id).collect();
            for id in ids {
                app.remove_track(id);
            }
            app.loop_len = None;
            app.log(Cat::Track, "all tracks deleted, loop length reset");
        }
        "synth" => app.set_module(Module::Synth),
        "voice" => app.set_module(Module::Voice),
        "drums" | "drum" => {
            app.set_module(Module::Drums);
            if let Some(name) = args.first() {
                let inst = params::DRUMS
                    .iter()
                    .position(|d| d.eq_ignore_ascii_case(name))
                    .ok_or_else(|| format!("no drum '{name}' — kick, snare, hihat or cymbal"))?;
                app.drum_key(inst as u8, true);
            }
        }
        "bpm" | "tempo" => match args.first() {
            None => {
                let lock = if app.loop_len.is_some() {
                    " (locked to the loop)"
                } else {
                    ""
                };
                app.log(Cat::Drum, format!("tempo {:.1} bpm{lock}", app.bpm));
            }
            Some(s) => {
                let v: f32 = s
                    .parse()
                    .map_err(|_| format!("usage: bpm <{BPM_MIN}-{BPM_MAX}>"))?;
                app.set_bpm(v)?;
            }
        },
        "quit" | "q" | "exit" => app.quit = true,
        _ => return Err(format!("unknown command '{cmd}' — type 'help'")),
    }
    Ok(())
}

fn set(app: &mut App, args: &[&str]) -> Result<(), String> {
    const USAGE: &str =
        "usage: set <track#|live|synth|voice|master> <param> <value> [<param> <value> ...]";
    let first = args.first().ok_or(USAGE)?;
    let (t, rest) = match parse_target(app, first) {
        Some(t) => (t, &args[1..]),
        None if params::find(first).is_some() => (app.sel_target(), args),
        None => return Err(bad_target(app, first)),
    };
    if rest.is_empty() || rest.len() % 2 != 0 {
        return Err(USAGE.into());
    }
    let scope = app.scope_of(t).ok_or("no such target")?;
    let cur = *app.params_of(t).ok_or("no such target")?;
    let mut changes = Vec::new();
    for pair in rest.chunks(2) {
        let idx = params::find(pair[0]).ok_or_else(|| {
            format!(
                "unknown parameter '{}' — 'show {}' lists them",
                pair[0], first
            )
        })?;
        if !scope.applies(idx) {
            return Err(format!(
                "'{}' does not apply to {}",
                pair[0],
                app.label_of(t)
            ));
        }
        let text = pair[1];
        let num = |s: &str| {
            s.parse::<f32>()
                .map_err(|_| format!("'{text}' is not a number"))
        };
        let v = if let Some(r) = text.strip_prefix('*') {
            cur[idx] * num(r)?
        } else if let Some(r) = text.strip_prefix("+=") {
            cur[idx] + num(r)?
        } else if let Some(r) = text.strip_prefix("-=") {
            cur[idx] - num(r)?
        } else {
            params::parse_value(idx, text)?
        };
        changes.push((idx, v));
    }
    for (idx, v) in changes {
        app.set_param(t, idx, v)?;
    }
    Ok(())
}

fn reset(app: &mut App, args: &[&str]) -> Result<(), String> {
    let t = target_arg(app, args.first())?;
    if args.len() <= 1 {
        let mut d = params::defaults();
        // Keep what a target *is* (monitoring state, drum instrument and
        // pattern); reset only how it sounds.
        if let Some(cur) = app.params_of(t) {
            for i in [
                params::p::MONITOR,
                params::p::DRUM,
                params::p::RATE,
                params::p::OFFSET,
            ] {
                d[i] = cur[i];
            }
        }
        app.paste_params(t, &d)?;
        return Ok(());
    }
    for name in &args[1..] {
        let idx = params::find(name).ok_or_else(|| format!("unknown parameter '{name}'"))?;
        app.set_param(t, idx, params::SPECS[idx].default)?;
    }
    Ok(())
}

pub fn set_mute(app: &mut App, id: u32, value: Option<bool>) {
    let Some(i) = app.track_index(id) else { return };
    let m = value.unwrap_or(!app.tracks[i].muted);
    app.tracks[i].muted = m;
    app.send(Cmd::Mute(id, m));
    let name = app.tracks[i].name.clone();
    app.log(
        Cat::Track,
        format!(
            "track {} '{name}' {}",
            i + 1,
            if m { "muted" } else { "unmuted" }
        ),
    );
}

pub fn set_solo(app: &mut App, id: u32, value: Option<bool>) {
    let Some(i) = app.track_index(id) else { return };
    let s = value.unwrap_or(!app.tracks[i].solo);
    app.tracks[i].solo = s;
    app.send(Cmd::Solo(id, s));
    let name = app.tracks[i].name.clone();
    app.log(
        Cat::Track,
        format!(
            "track {} '{name}' {}",
            i + 1,
            if s { "solo" } else { "unsolo" }
        ),
    );
}

fn list_tracks(app: &mut App) {
    if app.tracks.is_empty() {
        app.log(Cat::Track, "no tracks yet — press Space to record a loop");
        return;
    }
    let mut out = String::from(" #  name               kind   length   state   sound");
    let any_solo = app.tracks.iter().any(|t| t.solo);
    for (i, t) in app.tracks.iter().enumerate() {
        let state = if t.solo {
            "solo"
        } else if t.muted {
            "muted"
        } else if any_solo {
            "(off)"
        } else {
            "▶"
        };
        let p = &t.params;
        use params::p as ix;
        let sound = match t.scope() {
            Scope::Synth { .. } => format!(
                "{}+{} cutoff {} lvl {}",
                params::format(ix::WAVE, p[ix::WAVE]),
                params::format(ix::WAVE2, p[ix::WAVE2]),
                params::format(ix::CUTOFF, p[ix::CUTOFF]),
                params::format(ix::LEVEL, p[ix::LEVEL])
            ),
            Scope::Drum { .. } => format!(
                "{} {} +{} tune {} lvl {}",
                params::format(ix::DRUM, p[ix::DRUM]),
                params::format(ix::RATE, p[ix::RATE]),
                params::format(ix::OFFSET, p[ix::OFFSET]),
                params::format(ix::TUNE, p[ix::TUNE]),
                params::format(ix::LEVEL, p[ix::LEVEL])
            ),
            _ => format!(
                "pitch {} reverse {} lvl {}",
                params::format(ix::PITCH, p[ix::PITCH]),
                params::format(ix::REVERSE, p[ix::REVERSE]),
                params::format(ix::LEVEL, p[ix::LEVEL])
            ),
        };
        out.push_str(&format!(
            "\n{:>2}  {:<18} {:<6} {:>6.2}s  {:<7} {sound}",
            i + 1,
            t.name.chars().take(18).collect::<String>(),
            t.kind.label(),
            app.secs(t.len),
            state
        ));
    }
    if let Some(l) = app.loop_len {
        out.push_str(&format!(
            "\nloop length {:.2} s · {:.1} bpm",
            app.secs(l),
            app.bpm
        ));
    }
    app.log(Cat::Track, out);
}

pub fn show_params(app: &mut App, t: Target) {
    let (Some(scope), Some(prm)) = (app.scope_of(t), app.params_of(t).copied()) else {
        app.log(Cat::Err, "no such target");
        return;
    };
    let mut out = format!("{} parameters (* = changed from default):", app.label_of(t));
    for page in scope.pages() {
        out.push_str(&format!("\n  [{}]", page.title));
        for idx in page.params {
            let s = &params::SPECS[idx];
            let mark = if (prm[idx] - s.default).abs() > 1e-6 {
                '*'
            } else {
                ' '
            };
            out.push_str(&format!(
                "\n  {mark} {:<9} {:<11} {}",
                s.name,
                params::format(idx, prm[idx]),
                s.help
            ));
        }
    }
    app.log(Cat::Ctrl, out);
}

fn preset(app: &mut App, args: &[&str]) -> Result<(), String> {
    const USAGE: &str =
        "usage: preset save <name> [target] · preset load <name> [target] · presets";
    match args.first().copied() {
        None | Some("list") | Some("ls") => {
            let list = session::list_presets();
            let msg = if list.is_empty() {
                "no presets yet".to_string()
            } else {
                list.join("  ")
            };
            app.log(
                Cat::Sess,
                format!("presets in {}:\n{msg}", session::presets_dir().display()),
            );
        }
        Some(op @ ("save" | "load")) => {
            let name = args.get(1).ok_or(USAGE)?;
            if !session::valid_name(name) {
                return Err("preset names may contain letters, digits, - _ . only".into());
            }
            let t = target_arg(app, args.get(2))?;
            let scope = app.scope_of(t).ok_or("no such target")?;
            let kind = match scope {
                Scope::Synth { .. } => "synth",
                Scope::Voice { .. } => "voice",
                Scope::Drum { .. } => "drum",
                Scope::Master => "master",
            };
            if op == "save" {
                let prm = app.params_of(t).ok_or("no such target")?;
                let path = session::save_preset(name, kind, params::to_map(prm, scope))?;
                let label = app.label_of(t);
                app.log(
                    Cat::Sess,
                    format!("preset '{name}' saved from {label} → {}", path.display()),
                );
            } else {
                let file = session::load_preset(name)?;
                if file.kind != kind {
                    app.log(
                        Cat::Warn,
                        format!(
                            "preset '{name}' is a {} sound, applying the shared parameters only",
                            file.kind
                        ),
                    );
                }
                let mut prm = *app.params_of(t).ok_or("no such target")?;
                params::apply_map(&mut prm, &file.params);
                app.paste_params(t, &prm)?;
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn midi(app: &mut App, args: &[&str]) -> Result<(), String> {
    match args.first().copied() {
        None => {
            let ports = crate::midi::Midi::list_ports();
            let status = match &app.midi.port {
                Some(p) => format!("connected to '{p}'"),
                None if app.midi.enabled => "not connected (scanning every 2 s)".into(),
                None => "disabled".into(),
            };
            let mut msg = format!("MIDI {status}");
            if ports.is_empty() {
                msg.push_str("\nno MIDI inputs found — is the piano on and plugged in via USB?");
            }
            for (i, p) in ports.iter().enumerate() {
                msg.push_str(&format!("\n  {}: {p}", i + 1));
            }
            app.log(Cat::Midi, msg);
        }
        Some("off") => {
            app.midi.disconnect();
            app.midi.enabled = false;
            app.log(Cat::Midi, "MIDI input disabled");
        }
        Some(sel) => {
            let ports = crate::midi::Midi::list_ports();
            let name = match sel.parse::<usize>() {
                Ok(i) => ports
                    .get(i.wrapping_sub(1))
                    .cloned()
                    .ok_or("no such MIDI port (see 'midi')")?,
                Err(_) => args.join(" "),
            };
            app.midi.set_filter(Some(name.clone()));
            app.log(
                Cat::Midi,
                format!("looking for a MIDI input matching '{name}'…"),
            );
        }
    }
    Ok(())
}

fn loop_cmd(app: &mut App, args: &[&str]) -> Result<(), String> {
    match args.first().copied() {
        None => {
            let msg = match app.loop_len {
                Some(l) => format!(
                    "loop length {:.3} s ({l} frames @ {} Hz)",
                    app.secs(l),
                    app.sr
                ),
                None => "no loop length yet — the first recorded loop sets it".into(),
            };
            app.log(Cat::Loop, msg);
        }
        Some("clear") => {
            if !app.tracks.is_empty() {
                return Err("delete all tracks first (or 'clear all')".into());
            }
            app.loop_len = None;
            app.log(
                Cat::Loop,
                "loop length cleared — the next loop sets a new one",
            );
        }
        Some(s) => {
            let secs: f32 = s
                .trim_end_matches('s')
                .parse()
                .map_err(|_| "usage: loop [clear | <seconds>]")?;
            if !app.tracks.is_empty() {
                return Err("the loop length can only be set while there are no tracks".into());
            }
            if !(0.1..=600.0).contains(&secs) {
                return Err("loop length must be between 0.1 and 600 seconds".into());
            }
            app.loop_len = Some((secs * app.sr as f32).round() as u64);
            app.loop_t0 = 0;
            app.log(Cat::Loop, format!("loop length set to {secs:.3} s"));
        }
    }
    Ok(())
}
