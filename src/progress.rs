//! Terminal listener: draws progress bars for engine events, or plain lines when stderr isn't
//! a terminal.

use crate::events::{Event, Level, Sink};
use crate::extract::human;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    overall: Option<ProgressBar>,
    bars: HashMap<usize, (ProgressBar, u64)>,
}

pub struct Terminal {
    mp: MultiProgress,
    state: Mutex<State>,
}

impl Terminal {
    fn say(&self, msg: String) {
        if self.mp.is_hidden() {
            eprintln!("{msg}");
        } else {
            let _ = self.mp.println(msg);
        }
    }

    fn handle(&self, e: Event) {
        let mut st = self.state.lock().unwrap();
        match e {
            // Several extractions can run side by side (an install script's early ones): their
            // plans add up on one overall bar.
            Event::Plan { compressed, .. } if st.overall.is_some() => {
                if let Some(pb) = &st.overall {
                    pb.inc_length(compressed);
                }
            }
            Event::Plan { compressed, .. } => {
                let pb = self.mp.add(ProgressBar::new(compressed));
                pb.set_style(
                    ProgressStyle::with_template(
                        "{prefix:>14} [{bar:40}] {bytes}/{total_bytes} compressed  {binary_bytes_per_sec}  ETA {eta}",
                    )
                    .unwrap()
                    .progress_chars("=> "),
                );
                pb.set_prefix("total");
                pb.enable_steady_tick(Duration::from_millis(500));
                st.overall = Some(pb);
            }
            Event::JobStarted { id, label, method, compressed, .. } => {
                let pb = match &st.overall {
                    Some(o) => self.mp.insert_before(o, ProgressBar::new(compressed)),
                    None => self.mp.add(ProgressBar::new(compressed)),
                };
                pb.set_style(
                    ProgressStyle::with_template("{prefix:>14} [{bar:40}] {bytes}/{total_bytes}  {binary_bytes_per_sec}  {msg}")
                        .unwrap()
                        .progress_chars("=> "),
                );
                pb.set_prefix(label);
                pb.set_message(method);
                st.bars.insert(id, (pb, 0));
            }
            Event::JobProgress { id, done } => {
                let delta = match st.bars.get_mut(&id) {
                    Some((pb, last)) if done > *last => {
                        pb.set_position(done);
                        let d = done - *last;
                        *last = done;
                        d
                    }
                    _ => 0,
                };
                if let Some(o) = &st.overall {
                    o.inc(delta);
                }
            }
            Event::JobFinished { id, label, files, output, secs, codecs, error } => {
                if let Some((pb, _)) = st.bars.remove(&id) {
                    pb.finish_and_clear();
                }
                drop(st);
                match error {
                    Some(err) => self.say(format!("{label}: FAILED: {err}")),
                    None => {
                        let mut line = format!("{label}: {files} files, {} in {secs:.0}s", human(output));
                        for c in &codecs {
                            match (c.cpu_secs, c.cores()) {
                                (Some(cpu), Some(cores)) => line.push_str(&format!(
                                    "\n    {}: {cpu:.0}s cpu / {:.0}s wall ({cores:.1} cores)",
                                    c.label, c.wall_secs
                                )),
                                _ => line.push_str(&format!("\n    {}: ? cpu", c.label)),
                            }
                        }
                        self.say(line);
                    }
                }
            }
            Event::Phase { name } => {
                if let Some(o) = st.overall.take() {
                    o.finish_and_clear();
                }
                drop(st);
                self.say(format!("== {name} =="));
            }
            Event::Message { level, text } => {
                drop(st);
                self.say(match level {
                    Level::Info => text,
                    Level::Warn => format!("warning: {text}"),
                    Level::Error => format!("error: {text}"),
                });
            }
        }
    }

    pub fn finish(&self) {
        if let Some(o) = self.state.lock().unwrap().overall.take() {
            o.finish_and_clear();
        }
    }
}

/// A sink that draws to the terminal, plus a handle to clear the bars at the end.
pub fn terminal() -> (Sink, Arc<Terminal>) {
    let t = Arc::new(Terminal { mp: MultiProgress::new(), state: Mutex::new(State::default()) });
    let t2 = t.clone();
    (Arc::new(move |e| t2.handle(e)), t)
}
