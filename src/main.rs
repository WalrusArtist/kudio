//! kudio — turn your computer keyboard into an instrument.
//!
//! Thread layout (see docs/ARCHITECTURE.md):
//! * main/UI thread  — keyboard, terminal log, commands, sessions (`app`)
//! * audio output    — the real-time `engine` (cpal callback)
//! * audio input     — microphone capture into a ring buffer (cpal callback)
//! * MIDI input      — digital piano messages straight to the engine (midir)
//! * take writer     — streams the session recording to a WAV file

mod app;
mod audio;
mod commands;
mod devlog;
mod drums;
mod dsp;
mod engine;
mod keymap;
mod midi;
mod params;
mod session;
mod synth;
mod term;
mod track;

use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{cursor, execute, terminal};
use std::io::stdout;
use std::sync::mpsc;
use term::Cat;

const USAGE: &str = "\
kudio — turn your computer keyboard into an instrument

USAGE:
    kudio [SESSION] [OPTIONS]

ARGS:
    SESSION               session to open (created if it does not exist)
                          default: a new session-YYYYMMDD-HHMMSS

OPTIONS:
    -r, --record          record the whole performance to a WAV take from the start
        --midi <NAME>     use the MIDI input whose name contains NAME
        --no-midi         do not use MIDI input
        --no-mic          do not open the microphone (voice module disabled)
        --output <NAME>   audio output device (substring match, see --list-devices)
        --input <NAME>    microphone device (substring match)
        --buffer <N>      audio buffer in frames (default 256; lower = less latency)
        --list-devices    list audio devices and MIDI inputs, then exit
        --log[=PATH]      developer log: keys, events and messages to PATH
                          (default ./kudio.log)
    -h, --help            show this help

Data is stored in ~/Music/kudio (override with $KUDIO_HOME).
Inside kudio press F5 for the key map, Enter → 'help' for commands.";

#[derive(Default)]
struct Args {
    session: Option<String>,
    record: bool,
    midi: Option<String>,
    no_midi: bool,
    no_mic: bool,
    output: Option<String>,
    input: Option<String>,
    buffer: Option<u32>,
    list: bool,
    help: bool,
    log: Option<std::path::PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-r" | "--record" => a.record = true,
            "--midi" => a.midi = Some(value("--midi")?),
            "--no-midi" => a.no_midi = true,
            "--no-mic" => a.no_mic = true,
            "--output" => a.output = Some(value("--output")?),
            "--input" => a.input = Some(value("--input")?),
            "--buffer" => {
                let v = value("--buffer")?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("invalid buffer size '{v}'"))?;
                a.buffer = Some(n.clamp(32, 8192));
            }
            "--list-devices" => a.list = true,
            "--log" => a.log = Some("kudio.log".into()),
            s if s.starts_with("--log=") => {
                let path = &s["--log=".len()..];
                if path.is_empty() {
                    return Err("--log= needs a file path".into());
                }
                a.log = Some(path.into());
            }
            "-h" | "--help" => a.help = true,
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            s => {
                if a.session.is_some() {
                    return Err(format!("unexpected argument '{s}'"));
                }
                if !session::valid_name(s) {
                    return Err(format!(
                        "invalid session name '{s}' (letters, digits, - _ . only)"
                    ));
                }
                a.session = Some(s.to_string());
            }
        }
    }
    Ok(a)
}

fn restore_terminal(enhanced: bool) {
    let mut out = stdout();
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kudio: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return;
    }
    if args.list {
        println!("Audio devices:");
        for d in audio::list_devices() {
            println!("  {d}");
        }
        println!("MIDI inputs:");
        let ports = midi::Midi::list_ports();
        if ports.is_empty() {
            println!("  (none)");
        }
        for p in ports {
            println!("  {p}");
        }
        return;
    }
    if let Err(e) = run(args) {
        eprintln!("kudio: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    if let Some(path) = &args.log {
        devlog::init(path).map_err(|e| format!("cannot open log {}: {e}", path.display()))?;
        dlog!(
            "kudio {} started, args: {:?}",
            env!("CARGO_PKG_VERSION"),
            std::env::args().skip(1).collect::<Vec<_>>()
        );
        dlog!("TERM={:?}", std::env::var("TERM").ok());
    }
    let session_name = args
        .session
        .clone()
        .unwrap_or_else(session::default_session_name);
    let (sys_tx, sys_rx) = mpsc::channel::<String>();

    let opts = audio::AudioOptions {
        output: args.output.clone(),
        input: args.input.clone(),
        buffer: args.buffer.unwrap_or(256),
        mic: !args.no_mic,
    };
    let (audio, mut link) = audio::start(&opts, sys_tx.clone())?;
    let sr = audio.sample_rate;
    dlog!(
        "audio: out={:?} in={:?} sr={sr} buffer={:?}",
        audio.out_name,
        audio.in_name,
        audio.buffer
    );

    let (midi_log_tx, midi_log_rx) = mpsc::channel();
    let midi_tx = link
        .midi_cmd
        .take()
        .expect("MIDI queue is created with the engine");
    let midi = midi::Midi::new(midi_tx, midi_log_tx, args.midi.clone(), !args.no_midi);

    // Terminal: raw mode + kitty keyboard protocol for key-release events.
    terminal::enable_raw_mode().map_err(|e| format!("cannot use this terminal: {e}"))?;
    let probe = terminal::supports_keyboard_enhancement();
    dlog!("keyboard enhancement probe: {probe:?}");
    let enhanced = probe.unwrap_or(false);
    if enhanced {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }
    let _ = execute!(stdout(), cursor::Hide);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        dlog!("PANIC: {info}");
        devlog::flush();
        restore_terminal(enhanced);
        default_hook(info);
    }));

    let mut app = app::App::new(app::AppInit {
        link,
        sr,
        midi,
        midi_log: midi_log_rx,
        sys_log: sys_rx,
        sys_tx,
        enhanced,
        mic_ok: audio.in_name.is_some(),
        session: session_name.clone(),
    });

    app.log(
        Cat::Sys,
        format!(
            "kudio {} · data in {}",
            env!("CARGO_PKG_VERSION"),
            session::root().display()
        ),
    );
    let latency = match audio.buffer {
        Some(b) => format!("buffer {b} ({:.1} ms)", b as f32 * 1000.0 / sr as f32),
        None => "buffer: device default".to_string(),
    };
    app.log(
        Cat::Sys,
        format!("audio out: {} @ {sr} Hz, {latency}", audio.out_name),
    );
    match &audio.in_name {
        Some(n) => app.log(
            Cat::Sys,
            format!("mic: {n}  (monitoring off — F4 to hear yourself, use headphones)"),
        ),
        None => app.log(Cat::Warn, "no microphone: voice module disabled"),
    }
    if enhanced {
        app.log(Cat::Sys, "keyboard: key press + release detected ✓");
    } else {
        app.log(
            Cat::Warn,
            format!(
                "this terminal does not report key releases: notes last {:.2} s ('gate' to change). \
                 Use kitty, ghostty, alacritty, foot or wezterm for real note lengths.",
                app.gate
            ),
        );
    }
    if args.no_midi {
        app.log(Cat::Midi, "MIDI disabled");
    } else {
        app.log(
            Cat::Midi,
            "watching for a USB MIDI keyboard (plug it in any time)",
        );
    }

    if let Some(path) = &args.log {
        app.log(Cat::Sys, format!("developer log → {}", path.display()));
    }

    if session::exists(&session_name) {
        match session::load(&session_name, sr) {
            Ok(l) => {
                let n = l.tracks.len();
                app.apply_loaded(l);
                app.log(
                    Cat::Sess,
                    format!("opened session '{session_name}' · {n} tracks"),
                );
            }
            Err(e) => app.log(
                Cat::Err,
                format!("could not load session '{session_name}': {e}"),
            ),
        }
    } else {
        app.log(Cat::Sess, format!("new session '{session_name}'"));
    }
    if args.record {
        app.toggle_take();
    }
    app.log(
        Cat::Help,
        "Play with z..m / q..p · Space = record a loop · F3 = drums · Tab = control mode · F5 = help",
    );

    let result = app.run();

    app.save();
    app.term.finish();
    let takes = app.into_takes();
    // Stopping the audio drops the engine and with it the take producers,
    // which tells the writer threads to finalize their WAV files.
    drop(audio);
    for t in takes {
        t.join();
    }
    restore_terminal(enhanced);
    dlog!("exit: {result:?}");
    devlog::flush();
    println!(
        "bye! session '{session_name}' saved in {}",
        session::session_dir(&session_name).display()
    );
    result.map_err(|e| e.to_string())
}
