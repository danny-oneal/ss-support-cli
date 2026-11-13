//! The process streams as shareable handles, so the pipeline, progress
//! callbacks, and confirmation prompts can all write to the same stderr and
//! tests can capture everything in memory.

use std::fmt;
use std::io::{self, BufRead, BufReader, Cursor, IsTerminal, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// A cloneable, thread-safe output stream. Clones write to the same
/// underlying writer.
#[derive(Clone)]
pub struct Sink(Arc<Mutex<Box<dyn Write + Send>>>);

impl Sink {
    /// Wraps `w` so it can be shared.
    pub fn new(w: impl Write + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(w))))
    }

    fn lock(&self) -> MutexGuard<'_, Box<dyn Write + Send>> {
        // A writer poisoned by a panicking holder is still a usable writer.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.lock().write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.lock().write_all(buf)
    }

    /// Formats the whole message first and writes it under one lock, so a
    /// line never interleaves with another writer's.
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> io::Result<()> {
        match args.as_str() {
            Some(s) => self.write_all(s.as_bytes()),
            None => self.write_all(args.to_string().as_bytes()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock().flush()
    }
}

/// A cloneable, thread-safe line reader. Clones read from the same
/// underlying stream.
#[derive(Clone)]
pub struct Source(Arc<Mutex<Box<dyn BufRead + Send>>>);

impl Source {
    /// Wraps `r` so it can be shared.
    pub fn new(r: impl BufRead + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(r))))
    }

    /// Reads the next line without its line ending; `None` at end of input.
    pub fn read_line(&self) -> io::Result<Option<String>> {
        let mut line = String::new();
        let n = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(Some(line))
    }
}

/// The streams one invocation reads from and writes to: results on stdout,
/// diagnostics on stderr, answers to prompts on stdin.
#[derive(Clone)]
pub struct Io {
    /// Receives results.
    pub stdout: Sink,
    /// Receives errors, progress, and prompts.
    pub stderr: Sink,
    /// Supplies answers to prompts.
    pub stdin: Source,
    /// Whether an operator is at a terminal on both ends: stdin and stdout
    /// are terminals. Only then can the interactive front-end be shown.
    pub is_terminal: bool,
}

impl Io {
    /// The process's standard streams.
    pub fn std() -> Self {
        Self {
            stdout: Sink::new(io::stdout()),
            stderr: Sink::new(io::stderr()),
            stdin: Source::new(BufReader::new(io::stdin())),
            is_terminal: io::stdin().is_terminal() && io::stdout().is_terminal(),
        }
    }

    /// In-memory streams: stdin yields `stdin`, and everything written is
    /// kept in the returned [`Captured`].
    pub fn capture(stdin: &str) -> (Self, Captured) {
        let captured = Captured::default();
        let io = Self {
            stdout: Sink::new(Buffer(Arc::clone(&captured.stdout))),
            stderr: Sink::new(Buffer(Arc::clone(&captured.stderr))),
            stdin: Source::new(Cursor::new(stdin.as_bytes().to_vec())),
            is_terminal: false,
        };
        (io, captured)
    }
}

/// What an [`Io::capture`] pair has written so far.
#[derive(Clone, Default)]
pub struct Captured {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Captured {
    /// Everything written to stdout, lossily decoded as UTF-8.
    pub fn stdout(&self) -> String {
        contents(&self.stdout)
    }

    /// Everything written to stderr, lossily decoded as UTF-8.
    pub fn stderr(&self) -> String {
        contents(&self.stderr)
    }
}

fn contents(buf: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
}

/// An in-memory writer shared with a [`Captured`].
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_clone<T: Send + Sync + Clone>() {}

    #[test]
    fn handles_are_shareable_across_threads() {
        assert_send_clone::<Sink>();
        assert_send_clone::<Source>();
        assert_send_clone::<Io>();
    }

    #[test]
    fn clones_write_to_the_same_capture() {
        let (io, captured) = Io::capture("");
        let mut a = io.stderr.clone();
        let mut b = io.stderr;
        write!(a, "one ").expect("write");
        std::thread::spawn(move || write!(b, "two").expect("write"))
            .join()
            .expect("thread");
        assert_eq!(captured.stderr(), "one two");
        assert_eq!(captured.stdout(), "");
    }

    /// Records every `write` call as its own chunk.
    struct Chunks(Arc<Mutex<Vec<String>>>);

    impl Write for Chunks {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("lock")
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn one_formatted_line_is_one_write() {
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let mut sink = Sink::new(Chunks(Arc::clone(&chunks)));
        let (step, n) = ("step", 3);
        writeln!(sink, "→ {step} ({n:?})").expect("write");
        assert_eq!(*chunks.lock().expect("lock"), ["→ step (3)\n"]);
    }

    #[test]
    fn read_line_strips_endings_and_ends_with_none() {
        let (io, _) = Io::capture("y\r\nno\nlast");
        let stdin = io.stdin.clone();
        assert_eq!(stdin.read_line().expect("read").as_deref(), Some("y"));
        assert_eq!(io.stdin.read_line().expect("read").as_deref(), Some("no"));
        assert_eq!(stdin.read_line().expect("read").as_deref(), Some("last"));
        assert_eq!(io.stdin.read_line().expect("read"), None);
    }
}
