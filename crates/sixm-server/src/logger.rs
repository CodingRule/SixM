//! Tiny leveled console logger for the SixM server.
//!
//! Colour is used only when the target stream is a TTY and `NO_COLOR` is unset
//! (<https://no-color.org>: any non-empty value disables colour).

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::io::{IsTerminal, Write};
use std::rc::Rc;
use std::str::FromStr;

/// Log severity, in increasing order. `Silent` is a threshold only: nothing is logged at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
    Silent,
}

impl Level {
    /// Lower-case name, as accepted by `SIXM_LOG_LEVEL`.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Silent => "silent",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            Level::Debug => "\x1b[90m",
            Level::Info => "\x1b[36m",
            Level::Warn => "\x1b[33m",
            Level::Error | Level::Silent => "\x1b[31m",
        }
    }
}

impl FromStr for Level {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "debug" => Ok(Level::Debug),
            "info" => Ok(Level::Info),
            "warn" => Ok(Level::Warn),
            "error" => Ok(Level::Error),
            "silent" => Ok(Level::Silent),
            _ => Err(()),
        }
    }
}

/// Which standard stream a line is meant for: debug/info go to stdout, warn/error to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Lines captured by [`Logger::capture`], oldest first, each ending in `\n`.
pub type Captured = Rc<RefCell<Vec<String>>>;

type SinkFn = dyn Fn(Stream, &str);

#[derive(Clone)]
enum Sink {
    Stdio,
    Custom { write: Rc<SinkFn>, tty: bool },
}

/// A cheap-to-clone logger handle. Clones share one level; a [`child`](Logger::child) has its own.
#[derive(Clone)]
pub struct Logger {
    name: Rc<str>,
    level: Rc<Cell<Level>>,
    sink: Sink,
}

impl Logger {
    /// Logs to stdout/stderr at the level named by `SIXM_LOG_LEVEL` (default `info`).
    /// `name` is shown in brackets; pass `""` for none.
    pub fn new(name: &str) -> Logger {
        let level = std::env::var("SIXM_LOG_LEVEL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(Level::Info);
        Logger {
            name: name.into(),
            level: Rc::new(Cell::new(level)),
            sink: Sink::Stdio,
        }
    }

    /// A logger that discards everything (handy in tests).
    pub fn silent() -> Logger {
        Logger::new("").with_level(Level::Silent)
    }

    /// Sends every line to `write` instead of the console, at level `debug`.
    /// `tty` says whether the sink should be treated as a terminal for colouring.
    pub fn to_sink(name: &str, tty: bool, write: impl Fn(Stream, &str) + 'static) -> Logger {
        Logger {
            name: name.into(),
            level: Rc::new(Cell::new(Level::Debug)),
            sink: Sink::Custom {
                write: Rc::new(write),
                tty,
            },
        }
    }

    /// A logger at level `debug` that records its (uncoloured) lines instead of printing them.
    pub fn capture() -> (Logger, Captured) {
        let lines = Captured::default();
        let sink = lines.clone();
        let logger = Logger::to_sink("", false, move |_, line| {
            sink.borrow_mut().push(line.to_string())
        });
        (logger, lines)
    }

    /// Builder-style level override.
    pub fn with_level(self, level: Level) -> Logger {
        self.level.set(level);
        self
    }

    pub fn level(&self) -> Level {
        self.level.get()
    }

    /// Changes the level of this logger and its clones (not of children already created).
    pub fn set_level(&self, level: Level) {
        self.level.set(level);
    }

    /// A logger with the same destination and level but a nested prefix (`parent:child`).
    pub fn child(&self, name: &str) -> Logger {
        let name = if self.name.is_empty() {
            name.to_string()
        } else {
            format!("{}:{name}", self.name)
        };
        Logger {
            name: name.into(),
            level: Rc::new(Cell::new(self.level.get())),
            sink: self.sink.clone(),
        }
    }

    pub fn debug(&self, message: impl AsRef<str>) {
        self.write(Level::Debug, message.as_ref());
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.write(Level::Info, message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.write(Level::Warn, message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.write(Level::Error, message.as_ref());
    }

    /// Whether a message at `level` would be written; lets callers skip building expensive ones.
    pub fn enabled(&self, level: Level) -> bool {
        level >= self.level.get()
    }

    fn write(&self, level: Level, text: &str) {
        if !self.enabled(level) {
            return;
        }
        let stream = if level >= Level::Warn {
            Stream::Stderr
        } else {
            Stream::Stdout
        };
        let tty = match &self.sink {
            Sink::Stdio => match stream {
                Stream::Stdout => std::io::stdout().is_terminal(),
                Stream::Stderr => std::io::stderr().is_terminal(),
            },
            Sink::Custom { tty, .. } => *tty,
        };
        let colour = colour_enabled(tty, std::env::var_os("NO_COLOR").as_deref());
        let line = format_line(&timestamp(), level, &self.name, text, colour);
        match &self.sink {
            // A broken stdout must never take the server down, so write errors are ignored.
            Sink::Stdio => match stream {
                Stream::Stdout => drop(std::io::stdout().lock().write_all(line.as_bytes())),
                Stream::Stderr => drop(std::io::stderr().lock().write_all(line.as_bytes())),
            },
            Sink::Custom { write, .. } => write(stream, &line),
        }
    }
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("name", &self.name)
            .field("level", &self.level.get())
            .finish()
    }
}

fn colour_enabled(tty: bool, no_color: Option<&OsStr>) -> bool {
    tty && no_color.map_or(true, OsStr::is_empty)
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn format_line(time: &str, level: Level, name: &str, text: &str, colour: bool) -> String {
    const RESET: &str = "\x1b[0m";
    const DIM: &str = "\x1b[2m";
    let tag = format!("{:<5}", level.as_str().to_uppercase());
    let scope = if name.is_empty() {
        String::new()
    } else {
        format!(" [{name}]")
    };
    if colour {
        format!(
            "{DIM}{time}{RESET} {}{tag}{RESET}{DIM}{scope}{RESET} {text}\n",
            level.colour()
        )
    } else {
        format!("{time} {tag}{scope} {text}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_needs_a_tty_and_no_no_color() {
        assert!(colour_enabled(true, None));
        assert!(colour_enabled(true, Some(OsStr::new(""))));
        assert!(!colour_enabled(true, Some(OsStr::new("1"))));
        assert!(!colour_enabled(false, None));
    }

    #[test]
    fn lines_have_the_documented_shape() {
        assert_eq!(
            format_line("12:34:56", Level::Info, "srv", "hello", false),
            "12:34:56 INFO  [srv] hello\n"
        );
        assert_eq!(
            format_line("12:34:56", Level::Error, "", "bad", false),
            "12:34:56 ERROR bad\n"
        );
        let coloured = format_line("12:34:56", Level::Warn, "srv", "careful", true);
        assert_eq!(
            coloured,
            "\x1b[2m12:34:56\x1b[0m \x1b[33mWARN \x1b[0m\x1b[2m [srv]\x1b[0m careful\n"
        );
    }
}
