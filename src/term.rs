//! Terminal output: a scrolling, timestamped activity log with a sticky
//! multi-line status footer underneath. Plain crossterm, no TUI framework.

use crossterm::{
    cursor, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::io::{Stdout, Write};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cat {
    Synth,
    Voice,
    Drum,
    Loop,
    Ctrl,
    Midi,
    Track,
    Sess,
    Sys,
    Warn,
    Err,
    Cmd,
    Help,
}

impl Cat {
    fn tag(self) -> &'static str {
        match self {
            Cat::Synth => "SYNTH",
            Cat::Voice => "VOICE",
            Cat::Drum => "DRUM ",
            Cat::Loop => "LOOP ",
            Cat::Ctrl => "CTRL ",
            Cat::Midi => "MIDI ",
            Cat::Track => "TRACK",
            Cat::Sess => "SESS ",
            Cat::Sys => "SYS  ",
            Cat::Warn => "WARN ",
            Cat::Err => "ERROR",
            Cat::Cmd => "CMD  ",
            Cat::Help => "HELP ",
        }
    }
    fn color(self) -> Color {
        match self {
            Cat::Synth => Color::Cyan,
            Cat::Voice => Color::Magenta,
            Cat::Drum => Color::DarkMagenta,
            Cat::Loop => Color::Red,
            Cat::Ctrl => Color::Yellow,
            Cat::Midi => Color::Blue,
            Cat::Track => Color::Green,
            Cat::Sess => Color::DarkGreen,
            Cat::Sys => Color::Grey,
            Cat::Warn => Color::DarkYellow,
            Cat::Err => Color::DarkRed,
            Cat::Cmd => Color::White,
            Cat::Help => Color::DarkCyan,
        }
    }
}

pub struct Term {
    out: Stdout,
    start: Instant,
    footer: Vec<String>,
    drawn: usize,
    width: usize,
}

impl Term {
    pub fn new() -> Self {
        // Some ptys report 0×0; fall back to a sane width.
        let width = terminal::size()
            .ok()
            .map(|(w, _)| w as usize)
            .filter(|&w| w >= 20)
            .unwrap_or(100);
        Self {
            out: std::io::stdout(),
            start: Instant::now(),
            footer: Vec::new(),
            drawn: 0,
            width,
        }
    }

    pub fn resize(&mut self, w: u16) {
        if w >= 20 {
            self.width = w as usize;
        }
        self.redraw();
    }

    pub fn width(&self) -> usize {
        self.width
    }

    fn clear_footer(&mut self) {
        if self.drawn > 0 {
            if self.drawn > 1 {
                let _ = queue!(self.out, cursor::MoveUp((self.drawn - 1) as u16));
            }
            let _ = queue!(
                self.out,
                cursor::MoveToColumn(0),
                terminal::Clear(ClearType::FromCursorDown)
            );
            self.drawn = 0;
        }
    }

    fn draw_footer(&mut self) {
        let w = self.width.saturating_sub(1).max(10);
        let n = self.footer.len();
        for (i, line) in self.footer.iter().enumerate() {
            // Never wrap: wrapping would break the "move up N lines" redraw.
            let shown: String = line.chars().take(w).collect();
            let _ = queue!(
                self.out,
                SetAttribute(Attribute::Reverse),
                Print(shown),
                SetAttribute(Attribute::Reset)
            );
            if i + 1 < n {
                let _ = queue!(self.out, Print("\r\n"));
            }
        }
        self.drawn = n;
    }

    fn redraw(&mut self) {
        self.clear_footer();
        self.draw_footer();
        let _ = self.out.flush();
    }

    /// Replace the footer (redraws only if something changed).
    pub fn set_footer(&mut self, lines: Vec<String>) {
        if lines != self.footer {
            self.footer = lines;
            self.redraw();
        }
    }

    pub fn log(&mut self, cat: Cat, msg: impl AsRef<str>) {
        crate::dlog!("{} {}", cat.tag(), msg.as_ref());
        let t = self.start.elapsed();
        let secs = t.as_secs();
        let stamp = format!(
            "[{:02}:{:02}.{:03}]",
            secs / 60,
            secs % 60,
            t.subsec_millis()
        );
        self.clear_footer();
        let width = self.width.saturating_sub(1).max(20);
        for (i, line) in msg.as_ref().lines().enumerate() {
            let prefix_len = stamp.len() + 7;
            let body: String = line
                .chars()
                .take(width.saturating_sub(prefix_len))
                .collect();
            if i == 0 {
                let _ = queue!(
                    self.out,
                    SetForegroundColor(Color::DarkGrey),
                    Print(&stamp),
                    Print(" "),
                    SetForegroundColor(cat.color()),
                    SetAttribute(Attribute::Bold),
                    Print(cat.tag()),
                    SetAttribute(Attribute::Reset),
                    ResetColor,
                    Print(" "),
                    Print(body),
                    Print("\r\n"),
                );
            } else {
                let pad = " ".repeat(prefix_len);
                let _ = queue!(self.out, Print(pad), Print(body), Print("\r\n"));
            }
        }
        self.draw_footer();
        let _ = self.out.flush();
    }

    /// Clear the footer before handing the terminal back to the shell.
    pub fn finish(&mut self) {
        self.clear_footer();
        let _ = self.out.flush();
    }
}
