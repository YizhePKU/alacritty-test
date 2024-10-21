//! Helpers for tests that requires a terminal emulator. Terminal emulation is
//! supported by the `alacritty_terminal` crate.
//!
//! # Examples
//!
//! ```no_run
//! use alacritty_test::{extract_text, pty_spawn, EventedReadWrite, Terminal};
//! use std::{io::Write, time::Duration};
//!
//! // create a PTY and spawn a child process into the slave end
//! let mut pty = pty_spawn("target/debug/examples/basic", vec![], None);
//! let mut terminal = Terminal::new();
//!
//! // read the prompt from the application
//! terminal.read_from_pty(&mut pty, Some(Duration::from_millis(200)))?;
//!
//! // send keystrokes to the application (note that pressing ENTER is '\r')
//! pty.writer().write_all(b"Hello world!\r")?;
//! pty.writer().write_all(b"Second line!\r")?;
//!
//! // read the response from the application
//! terminal.read_from_pty(&mut pty, Some(Duration::from_millis(200)))?;
//!
//! // (optional) quit the application with Ctrl+C
//! pty.writer().write_all(b"\x03")?;
//! terminal.read_from_pty(&mut pty, None).unwrap_err();
//!
//! // render the content of the terminal
//! let term = terminal.into_inner();
//! let text = extract_text(&term);
//! for line in text {
//!     println!("{}", line);
//! }
//! ```

use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    grid::Indexed,
    term::{test::TermSize, Config},
    tty::{self, Options, Pty, Shell},
    vte::ansi::{Processor, StdSyncHandler},
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};
use unicode_width_16::UnicodeWidthChar;

pub use tty::EventedReadWrite;

pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

pub type Term = alacritty_terminal::Term<EventProxy>;

/// The terminal state.
pub struct Terminal {
    term: Term,
    events: mpsc::Receiver<Event>,
    event_handler: Box<dyn FnMut(&mut Term, &mut Pty, Event)>,
}

impl Terminal {
    /// Create a 24x80 terminal with default configurations.
    pub fn new() -> Self {
        let config = Config::default();
        let size = TermSize {
            screen_lines: 24,
            columns: 80,
        };
        let (tx, rx) = mpsc::channel();
        Self {
            term: Term::new(config, &size, EventProxy(tx)),
            events: rx,
            event_handler: Box::new(pty_write_handler),
        }
    }

    /// Set the event handler that handles incoming terminal events.
    pub fn set_event_handler<EventHandler>(&mut self, handler: EventHandler)
    where
        EventHandler: FnMut(&mut Term, &mut Pty, Event) + 'static,
    {
        self.event_handler = Box::new(handler);
    }

    /// Read from `pty` and updates the terminal state. If `timeout` is set,
    /// will return early if we haven't heard anything from the application for
    /// the timeout duration. If the application exits, will read all remaining
    /// data in the PTY buffer, then return an error.
    pub fn read_from_pty(
        &mut self,
        pty: &mut Pty,
        timeout: Option<Duration>,
    ) -> std::io::Result<()> {
        let poller = Arc::new(polling::Poller::new().expect("polling is available"));
        // SAFETY: Poller requires us to manually delete the source before
        // dropping, and we'll do just that.
        unsafe {
            let interest = polling::Event::readable(0);
            let mode = polling::PollMode::Level;
            pty.register(&poller, interest, mode)
                .expect("level-trigger polling is available");
        }

        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let mut _polling_events = polling::Events::new();
        loop {
            // Read from the PTY.
            let mut buf = [0; 1000];
            let n = pty.reader().read(&mut buf)?;

            // Poll if the read would block. On Windows, it's important to
            // always attempt a read before polling, otherwise
            // `alacritty_terminal` won't arrange for the event to be posted.
            if n == 0 {
                if poller.wait(&mut _polling_events, timeout)? > 0 {
                    continue;
                } else {
                    break;
                }
            }

            // Update the terminal state.
            for byte in &buf[..n] {
                parser.advance(&mut self.term, *byte);
            }

            // Handle terminal events.
            while let Ok(event) = self.events.try_recv() {
                (self.event_handler)(&mut self.term, pty, event);
            }
        }

        // Delete the source from the poller before dropping.
        pty.deregister(&poller).unwrap();

        Ok(())
    }

    /// Get a reference to the inner [alacritty_terminal::Term] instance.
    pub fn inner(&self) -> &Term {
        &self.term
    }

    /// Extract the inner [alacritty_terminal::Term] instance.
    pub fn into_inner(self) -> Term {
        self.term
    }
}

/// An event handler that only responds to `Event::PtyWrite`.
pub fn pty_write_handler(_terminal: &mut Term, pty: &mut Pty, event: Event) {
    if let Event::PtyWrite(text) = event {
        let _ = pty.writer().write_all(text.as_bytes());
    }
}

/// Create a PTY and spawn a child process into the slave end. If `pwd` is None,
/// the child process will inherit PWD from the current process.
pub fn pty_spawn(executable: &str, args: Vec<&str>, pwd: Option<PathBuf>) -> std::io::Result<Pty> {
    let options = Options {
        shell: Some(Shell::new(
            executable.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        )),
        working_directory: pwd,
        hold: false,
        env: HashMap::new(),
    };
    let window_size = WindowSize {
        num_lines: 24,
        num_cols: 80,
        cell_width: 0,
        cell_height: 0,
    };
    tty::new(&options, window_size, 0)
}

/// Extract the current cursor position in the terminal grid.
///
/// NOTE: the cursor column is NOT the same as its character offset due to the
/// existance of Unicode double-width characters.
pub fn extract_cursor<T>(terminal: &alacritty_terminal::Term<T>) -> (usize, usize) {
    let cursor = terminal.grid().cursor.point;
    (cursor.line.0 as usize, cursor.column.0)
}

/// Extract all visible text, ignoring text styles.
pub fn extract_text<T>(terminal: &alacritty_terminal::Term<T>) -> Vec<String> {
    let mut skip = 0;
    let mut text: Vec<String> = vec![];
    for Indexed { point, cell } in terminal.grid().display_iter() {
        if point.column == 0 {
            text.push(String::new());
        }

        // Skip over extra cells when double-width characters are encountered.
        if skip > 0 {
            skip -= 1;
            continue;
        }

        if let Some(width) = cell.c.width() {
            skip = width - 1;
        }

        text.last_mut()
            .expect("terminal grid start at column 0")
            .push(cell.c);
    }
    text
}
