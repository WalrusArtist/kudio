//! USB MIDI input (digital piano / controller keyboard).
//!
//! MIDI messages are parsed on midir's callback thread and forwarded straight
//! to the audio engine through a dedicated lock-free queue, so the piano does
//! not wait on the UI. A copy of each interesting message is sent to the UI
//! for logging.

use crate::engine::Cmd;
use midir::{Ignore, MidiInput, MidiInputConnection};
use rtrb::Producer;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the UI gets told about incoming MIDI.
#[derive(Debug, Clone, Copy)]
pub enum MidiLog {
    NoteOn(u8, u8),
    NoteOff(u8),
    Sustain(bool),
    Mod(u8),
}

pub enum Status {
    Connected(String),
    Disconnected(String),
    Error(String),
}

pub struct Midi {
    tx: Arc<Mutex<Producer<Cmd>>>,
    log: Sender<MidiLog>,
    conn: Option<MidiInputConnection<()>>,
    pub port: Option<String>,
    filter: Option<String>,
    pub enabled: bool,
    last_scan: Option<Instant>,
}

/// Parse a raw MIDI message into an engine command.
pub fn parse(msg: &[u8]) -> Option<(Cmd, Option<MidiLog>)> {
    let status = *msg.first()? & 0xF0;
    let d1 = *msg.get(1).unwrap_or(&0);
    let d2 = *msg.get(2).unwrap_or(&0);
    Some(match status {
        0x90 if d2 > 0 => (Cmd::NoteOn(d1, d2), Some(MidiLog::NoteOn(d1, d2))),
        0x80 | 0x90 => (Cmd::NoteOff(d1), Some(MidiLog::NoteOff(d1))),
        0xB0 => match d1 {
            64 => (Cmd::Sustain(d2 >= 64), Some(MidiLog::Sustain(d2 >= 64))),
            1 => (Cmd::ModWheel(d2 as f32 / 127.0), Some(MidiLog::Mod(d2))),
            120 | 123 => (Cmd::Panic, None),
            _ => return None,
        },
        0xE0 => {
            let v = ((d2 as i32) << 7 | d1 as i32) - 8192;
            (Cmd::Bend(v as f32 / 8192.0 * 2.0), None)
        }
        _ => return None,
    })
}

impl Midi {
    pub fn new(
        tx: Producer<Cmd>,
        log: Sender<MidiLog>,
        filter: Option<String>,
        enabled: bool,
    ) -> Self {
        Self {
            tx: Arc::new(Mutex::new(tx)),
            log,
            conn: None,
            port: None,
            filter,
            enabled,
            last_scan: None,
        }
    }

    pub fn list_ports() -> Vec<String> {
        // Without the ALSA sequencer device, alsa-lib prints an error to stderr
        // on every scan, which would garble the terminal UI every two seconds.
        #[cfg(target_os = "linux")]
        if !std::path::Path::new("/dev/snd/seq").exists() {
            return Vec::new();
        }
        let Ok(input) = MidiInput::new("kudio-scan") else {
            return Vec::new();
        };
        input
            .ports()
            .iter()
            .filter_map(|p| input.port_name(p).ok())
            .collect()
    }

    fn wanted(&self, name: &str) -> bool {
        match &self.filter {
            Some(f) => name.to_lowercase().contains(&f.to_lowercase()),
            None => !name.contains("Through"),
        }
    }

    pub fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter;
        self.enabled = true;
        self.disconnect();
        self.last_scan = None;
    }

    pub fn disconnect(&mut self) {
        if let Some(c) = self.conn.take() {
            c.close();
        }
        self.port = None;
    }

    /// Called regularly from the UI loop: connects when a device appears,
    /// notices when it disappears.
    pub fn poll(&mut self) -> Option<Status> {
        if !self.enabled {
            return None;
        }
        if self
            .last_scan
            .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return None;
        }
        self.last_scan = Some(Instant::now());
        let ports = Self::list_ports();
        if let Some(name) = &self.port {
            if !ports.iter().any(|p| p == name) {
                let name = name.clone();
                self.disconnect();
                return Some(Status::Disconnected(name));
            }
            return None;
        }
        let name = ports.into_iter().find(|n| self.wanted(n))?;
        Some(match self.connect(&name) {
            Ok(()) => Status::Connected(name),
            Err(e) => {
                // Do not retry a broken port every two seconds forever.
                self.enabled = false;
                Status::Error(format!(
                    "could not open MIDI port '{name}': {e} (type 'midi' to retry)"
                ))
            }
        })
    }

    fn connect(&mut self, name: &str) -> Result<(), String> {
        let mut input = MidiInput::new("kudio").map_err(|e| e.to_string())?;
        input.ignore(Ignore::All);
        let port = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).ok().as_deref() == Some(name))
            .ok_or("port vanished")?;
        let tx = self.tx.clone();
        let log = self.log.clone();
        let conn = input
            .connect(
                &port,
                "kudio-in",
                move |_, msg, _| {
                    if let Some((cmd, info)) = parse(msg) {
                        if let Ok(mut q) = tx.lock() {
                            let _ = q.push(cmd);
                        }
                        if let Some(info) = info {
                            let _ = log.send(info);
                        }
                    }
                },
                (),
            )
            .map_err(|e| e.to_string())?;
        self.conn = Some(conn);
        self.port = Some(name.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_note_on_off_and_pedal() {
        assert!(matches!(
            parse(&[0x90, 60, 100]),
            Some((Cmd::NoteOn(60, 100), _))
        ));
        assert!(matches!(parse(&[0x91, 60, 0]), Some((Cmd::NoteOff(60), _))));
        assert!(matches!(
            parse(&[0x80, 60, 40]),
            Some((Cmd::NoteOff(60), _))
        ));
        assert!(matches!(
            parse(&[0xB0, 64, 127]),
            Some((Cmd::Sustain(true), _))
        ));
        assert!(matches!(
            parse(&[0xB0, 64, 0]),
            Some((Cmd::Sustain(false), _))
        ));
        assert!(matches!(parse(&[0xE0, 0, 64]), Some((Cmd::Bend(b), _)) if b.abs() < 1e-6));
        assert!(parse(&[0xF8]).is_none());
    }
}
