//! Progress events. The engine reports what it is doing; the terminal progress bars and the
//! GUI window are both just listeners (a `Sink`).

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// CPU used by one codec process over its lifetime.
#[derive(Debug, Clone)]
pub struct CodecCpu {
    pub label: String,
    pub cpu_secs: Option<f64>,
    pub wall_secs: f64,
}

impl CodecCpu {
    /// Average number of busy cores.
    pub fn cores(&self) -> Option<f64> {
        self.cpu_secs.map(|c| c / self.wall_secs.max(0.001))
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// What is about to run.
    Plan { jobs: usize, compressed: u64, output: u64 },
    JobStarted { id: usize, label: String, method: String, compressed: u64, output: u64 },
    /// Compressed bytes this job has consumed so far.
    JobProgress { id: usize, done: u64 },
    JobFinished { id: usize, label: String, files: u64, output: u64, secs: f64, codecs: Vec<CodecCpu>, error: Option<String> },
    /// A new step of a multi-step operation (install), e.g. "Verifying archives".
    Phase { name: String },
    /// The compressed input of the whole install, known up front (script-driven installs).
    /// Later `Plan` events are then part of it, not additions.
    Total { compressed: u64 },
    /// The install script started step `index` (1-based) of `total`, in plain words.
    Step { index: usize, total: usize, label: String },
    /// Status line: notes, warnings, dismissed codec dialogs.
    Message { level: Level, text: String },
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

pub fn null_sink() -> Sink {
    Arc::new(|_| {})
}

pub fn info(sink: &Sink, text: impl Into<String>) {
    sink(Event::Message { level: Level::Info, text: text.into() });
}

pub fn warn(sink: &Sink, text: impl Into<String>) {
    sink(Event::Message { level: Level::Warn, text: text.into() });
}
