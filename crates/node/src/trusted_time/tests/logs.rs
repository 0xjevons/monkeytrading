//! Captures log lines so tests can check which alerts fired.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};
use tracing_subscriber::fmt;

/// Every line logged on this thread while its guard lives.
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    /// Appends `buf`.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    /// Nothing to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    /// Captures lines at `debug` and above on this thread until the guard drops. A current-thread
    /// runtime polls every spawned task on this thread, so their lines are captured too.
    pub fn capture() -> (Self, DefaultGuard) {
        crate::test_support::keep_callsites_interested();
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber =
            fmt().with_writer(move || writer.clone()).with_ansi(false).with_max_level(Level::DEBUG).finish();
        (logs, set_default(subscriber))
    }

    /// How many `error` lines, Critical alerts, raise `name`.
    pub fn critical(&self, name: &str) -> usize {
        self.alerts_at("ERROR", name)
    }

    /// How many `warn` lines, Warning alerts, raise `name`.
    pub fn warnings(&self, name: &str) -> usize {
        self.alerts_at("WARN", name)
    }

    /// How many lines at `level` raise the alert `name`.
    fn alerts_at(&self, level: &str, name: &str) -> usize {
        let (level, alert) = (format!(" {level} "), format!("alert=\"{name}\""));
        self.lines().filter(|l| l.contains(&level) && l.contains(&alert)).count()
    }

    /// How many lines contain `text`.
    pub fn count(&self, text: &str) -> usize {
        self.lines().filter(|l| l.contains(text)).count()
    }

    /// Every line captured so far.
    fn lines(&self) -> std::vec::IntoIter<String> {
        let text = String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned();
        text.lines().map(str::to_owned).collect::<Vec<_>>().into_iter()
    }
}
