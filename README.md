# kudio

Turn your computer keyboard into an instrument. kudio is a small (~1 MB),
terminal-only live looper for Linux, written in Rust:

- **Synth module**: an 8-voice polyphonic subtractive synth. Play it with
  your computer keyboard or a USB MIDI piano.
- **Voice module**: sing into your microphone, pitch-shift it live with the
  keyboard, and loop it.
- **Drums module**: kick, snare, hihat and cymbal, synthesized. Set a tempo
  and press `b`: a kick starts looping as a track on the beat, ready to
  tweak like any other track.
- **Looper**: press `Space` to record what you play next. It becomes a track
  that loops in the background while you build the next layer.
- **Two keyboard modes**: *piano* (play notes) and *control* (tweak sounds,
  mute, solo, delete tracks). `Tab` switches between them.
- **Command prompt** (`Enter`): edit any track by number, e.g. `2 cutoff 800`.
- **Everything is logged** to the terminal as you play.
- **Sessions & presets**: tracks and their sound settings are saved and can be
  reopened or reused in a later session. The whole performance can be recorded
  to a WAV file.

```text
[00:12.402] LOOP  ◌ armed: synth loop starts with your first note
[00:13.118] SYNTH ♪ C3   ( 48)  vel 100  ●rec
[00:16.871] LOOP  ■ track 1 'synth-1' recorded · loop length set to 3.75 s · now looping
[00:21.004] CMD   › 1 cutoff 800
[00:21.004] CTRL  track 1 'synth-1' · cutoff 3.00 kHz → 800.0 Hz
 SYNTH·PIANO │ loop 3.75s ▰▰▰▱▱▱▱▱ │ 1 trk │ out ▮▮▮··· │ mic ······ │ midi Digital Piano
```

## Build & run

kudio needs Rust 1.88+ (edition 2024) and the ALSA development headers
(`alsa-lib` on Arch/Omarchy, `libasound2-dev` on Debian/Ubuntu).

```sh
cargo build --release
./target/release/kudio myjam          # open (or create) the session "myjam"
./target/release/kudio myjam --record # ...and record the whole performance to WAV
./target/release/kudio --list-devices # show audio devices and MIDI inputs
./target/release/kudio myjam --log    # developer log of every key/event → ./kudio.log
```

> On this machine Rust was installed with rustup into `~/.cargo`. If `cargo`
> is not found, add `~/.cargo/bin` to your `PATH`.

Inside kudio press **F5** for the key map. Press **Enter** and type `help` to
see the commands.

## Terminal requirements

A computer keyboard is only a real instrument if kudio sees **key releases**
as well as presses. Use a terminal that supports the kitty keyboard protocol:
**kitty, ghostty, alacritty, foot or wezterm**. kudio prints
`keyboard: key press + release detected ✓` at startup when it works. Other
terminals (including most editor-embedded terminals) still work, but notes
then last a fixed time (the `gate` command sets it). Chords still hold while
you keep the keys down.

## Documentation

- [User manual](docs/USER_MANUAL.md): a walkthrough of a jam session, every
  key and command, and troubleshooting.
- [Architecture](docs/ARCHITECTURE.md): threads, signal flow, loop timing
  math, real-time safety and file formats, with diagrams.

## Data

Sessions, presets and recordings are stored in `~/Music/kudio`. Set
`$KUDIO_HOME` to use another location.

## Development

```sh
cargo test     # DSP, drums, engine, loop folding, parameter parsing, session round-trip
cargo clippy
cargo run --release -- jam --log   # then: tail -f kudio.log
```
