//! Collects what `tracing` events write, so a test can read the log lines.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// A log writer that keeps everything written to it.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// Routes `tracing` events on this thread into the capture until the guard drops.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// Everything logged so far.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'writer self) -> LogCapture {
        self.clone()
    }
}
