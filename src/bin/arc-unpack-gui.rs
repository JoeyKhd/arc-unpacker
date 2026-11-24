//! arc-unpack window: pick a repack folder and a target folder, press Install.
//! Launch it by double-clicking, or drop the repack folder onto the .exe.
#![windows_subsystem = "windows"]

use arc_unpack::events::{Event, Level, Sink};
use arc_unpack::extract::human;
use arc_unpack::install::{self, InstallOptions, InstallReport, Role};
use crossbeam_channel::{Receiver, unbounded};
use eframe::egui::{self, Color32, RichText};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn main() -> eframe::Result {
    arc_unpack::guard::kill_descendants_on_exit();
    arc_unpack::perf::tune_self();
    let start_dir = std::env::args_os().nth(1).map(PathBuf::from).filter(|p| p.is_dir());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 640.0]).with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("arc-unpack", options, Box::new(move |_cc| Ok(Box::new(App::new(start_dir)))))
}

struct ArchiveRow {
    name: String,
    packed: u64,
    unpacked: u64,
    role: Role,
    md5: bool,
}

struct ScanView {
    rows: Vec<ArchiveRow>,
    setup: bool,
    md5_list: bool,
    game_bytes: u64,
}

struct JobView {
    label: String,
    method: String,
    total: u64,
    done: u64,
    started: Instant,
    finished: Option<(f64, Option<String>, String)>, // secs, error, codec summary
}

#[derive(Default)]
struct Progress {
    phase: String,
    jobs: BTreeMap<usize, JobView>,
    plan_total: u64,
    log: Vec<(Level, String)>,
}

struct Running {
    events: Receiver<Event>,
    result: Receiver<anyhow::Result<InstallReport>>,
    cancel: Arc<AtomicBool>,
    skip_verify: Arc<AtomicBool>,
    started: Instant,
}

enum Outcome {
    Done { report: InstallReport, secs: f64 },
    Failed(String),
}

struct App {
    repack: String,
    target: String,
    scan: Option<Result<ScanView, String>>,
    scanned_for: String,
    /// Don't check the downloaded archives against the repack's MD5 list first.
    skip_check: bool,
    heavy_jobs: usize,
    progress: Progress,
    running: Option<Running>,
    outcome: Option<Outcome>,
    cpu: f64,
    ram: (u64, u64),
    threads: usize,
    last_sample: Instant,
}

fn default_target(repack: &Path) -> String {
    let name = repack.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // "Title [Some Repack]" -> "Title"
    let clean = name.split(" [").next().unwrap_or(&name).trim().to_string();
    let base = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    format!("{base}\\Games\\{}", if clean.is_empty() { "Game".into() } else { clean })
}

impl App {
    fn new(start_dir: Option<PathBuf>) -> Self {
        let mut app = App {
            repack: String::new(),
            target: String::new(),
            scan: None,
            scanned_for: String::new(),
            skip_check: false,
            heavy_jobs: 2,
            progress: Progress::default(),
            running: None,
            outcome: None,
            cpu: 0.0,
            ram: (0, 0),
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            last_sample: Instant::now() - Duration::from_secs(1),
        };
        if let Some(d) = start_dir {
            app.set_repack(d);
        }
        app
    }

    fn set_repack(&mut self, dir: PathBuf) {
        self.repack = dir.display().to_string();
        if self.target.is_empty() {
            self.target = default_target(&dir);
        }
    }

    fn rescan_if_needed(&mut self) {
        if self.repack.is_empty() || self.repack == self.scanned_for {
            return;
        }
        self.scanned_for = self.repack.clone();
        let dir = Path::new(&self.repack);
        let scanned = install::scan_prepared(dir, &install::default_cache_dir()).or_else(|_| install::scan(dir, None));
        self.scan = Some(match scanned {
            Ok(s) => Ok(ScanView {
                setup: s.setup.is_some(),
                md5_list: s.md5_file.is_some(),
                game_bytes: s.game_bytes(),
                rows: s
                    .archives
                    .iter()
                    .map(|a| ArchiveRow {
                        name: a.archive.name(),
                        packed: a.archive.len,
                        unpacked: a.archive.total_size(),
                        role: a.role,
                        md5: a.md5.is_some(),
                    })
                    .collect(),
            }),
            Err(e) => Err(format!("{e:#}")),
        });
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (etx, erx) = unbounded::<Event>();
        let (rtx, rrx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let skip_verify = Arc::new(AtomicBool::new(false));
        let repaint = ctx.clone();
        let sink: Sink = Arc::new(move |e| {
            let _ = etx.send(e);
            repaint.request_repaint();
        });
        let opts = InstallOptions {
            repack_dir: PathBuf::from(&self.repack),
            target_dir: PathBuf::from(&self.target),
            cache_dir: install::default_cache_dir(),
            config: None,
            verify_md5: !self.skip_check,
            jobs: 3,
            heavy_jobs: self.heavy_jobs,
            writers: 8,
            overwrite: true, // a re-run repairs / resumes the same folder
            stall_timeout: 600,
            tune: true,
            host_dir: std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default(),
            cancel: cancel.clone(),
            skip_verify: skip_verify.clone(),
            dry_run: false,
            script: true,
            components: None,
        };
        let done_ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = install::install(&opts, &sink);
            let _ = rtx.send(r);
            done_ctx.request_repaint();
        });
        self.progress = Progress::default();
        self.outcome = None;
        self.running = Some(Running { events: erx, result: rrx, cancel, skip_verify, started: Instant::now() });
    }

    fn pump(&mut self) {
        let Some(run) = &self.running else { return };
        while let Ok(e) = run.events.try_recv() {
            let p = &mut self.progress;
            match e {
                Event::Phase { name } => {
                    p.phase = name;
                    p.jobs.clear();
                    p.plan_total = 0;
                }
                Event::Plan { compressed, .. } => p.plan_total += compressed,
                Event::JobStarted { id, label, method, compressed, .. } => {
                    p.jobs.insert(id, JobView { label, method, total: compressed, done: 0, started: Instant::now(), finished: None });
                }
                Event::JobProgress { id, done } => {
                    if let Some(j) = p.jobs.get_mut(&id) {
                        j.done = done.max(j.done);
                    }
                }
                Event::JobFinished { id, secs, error, codecs, .. } => {
                    let summary = codecs
                        .iter()
                        .filter_map(|c| c.cores().map(|k| format!("{} {:.1} cores", c.label.split(' ').next().unwrap_or(""), k)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if let Some(j) = p.jobs.get_mut(&id) {
                        j.done = j.total;
                        j.finished = Some((secs, error, summary));
                    }
                }
                Event::Message { level, text } => p.log.push((level, text)),
            }
        }
        if let Ok(r) = run.result.try_recv() {
            let secs = run.started.elapsed().as_secs_f64();
            self.outcome = Some(match r {
                Ok(report) => Outcome::Done { report, secs },
                Err(e) => Outcome::Failed(format!("{e:#}")),
            });
            self.running = None;
        }
    }

    fn sample_system(&mut self) {
        if self.last_sample.elapsed() >= Duration::from_millis(1000) {
            let (cpu, used, total) = arc_unpack::guard::system_usage();
            self.cpu = cpu;
            self.ram = (used, total);
            self.last_sample = Instant::now();
        }
    }
}

fn folder_row(ui: &mut egui::Ui, label: &str, value: &mut String, enabled: bool) -> bool {
    let mut picked = false;
    ui.horizontal(|ui| {
        ui.add_sized([70.0, 20.0], egui::Label::new(label));
        let w = ui.available_width() - 80.0;
        ui.add_enabled(enabled, egui::TextEdit::singleline(value).desired_width(w));
        if ui.add_enabled(enabled, egui::Button::new("Browse…")).clicked()
            && let Some(d) = rfd::FileDialog::new().pick_folder()
        {
            *value = d.display().to_string();
            picked = true;
        }
    });
    picked
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        self.sample_system();
        self.rescan_if_needed();
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        let busy = self.running.is_some();

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.heading("arc-unpack");
            ui.label(RichText::new("Fast, parallel installer for FreeArc-based repacks").weak());
            ui.add_space(8.0);

            if folder_row(ui, "Repack", &mut self.repack, !busy) && self.target.is_empty() {
                self.target = default_target(Path::new(&self.repack));
            }
            folder_row(ui, "Install to", &mut self.target, !busy);
            ui.add_space(6.0);

            let mut can_install = false;
            match &self.scan {
                None => {
                    ui.label("Pick a repack folder (or drop it onto arc-unpack-gui.exe).");
                }
                Some(Err(e)) => {
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
                Some(Ok(s)) => {
                    egui::Grid::new("archives").striped(true).num_columns(4).show(ui, |ui| {
                        ui.strong("Archive");
                        ui.strong("Packed");
                        ui.strong("Unpacked");
                        ui.strong("Role");
                        ui.end_row();
                        for r in &s.rows {
                            ui.label(&r.name);
                            ui.label(human(r.packed));
                            ui.label(human(r.unpacked));
                            ui.label(format!("{}{}", r.role.describe(), if r.md5 { "" } else { " (no MD5)" }));
                            ui.end_row();
                        }
                    });
                    let free = arc_unpack::guard::free_space(Path::new(&self.target));
                    let enough = free.is_none_or(|f| f > s.game_bytes + (1 << 30));
                    ui.horizontal(|ui| {
                        ui.label(format!("Needs about {}", human(s.game_bytes)));
                        if let Some(f) = free {
                            let txt = format!("· {} free on the target drive", human(f));
                            if enough { ui.label(txt) } else { ui.colored_label(Color32::LIGHT_RED, txt) };
                        }
                    });
                    if !s.setup {
                        ui.colored_label(Color32::LIGHT_RED, "No Inno Setup installer (setup.exe) found: codecs can't be prepared.");
                    }
                    can_install = s.setup && enough && !self.target.trim().is_empty();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_enabled(!busy && s.md5_list, egui::Checkbox::new(&mut self.skip_check, "Skip archive check"))
                            .on_hover_text(
                                "Don't check the downloaded .bin files against the repack's MD5 list before installing. \
                                 Faster; the installed files are still checked as they are written.",
                            );
                        ui.separator();
                        ui.label("Heavy codec jobs at once:");
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.radio_value(&mut self.heavy_jobs, 1, "1");
                            ui.radio_value(&mut self.heavy_jobs, 2, "2 when RAM allows (default)");
                        });
                    });
                }
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if busy {
                    if ui.button("Cancel").clicked()
                        && let Some(r) = &self.running
                    {
                        r.cancel.store(true, Ordering::Relaxed);
                    }
                    let checking = self.progress.phase == "Verifying archives";
                    if let Some(r) = &self.running
                        && checking
                        && !r.skip_verify.load(Ordering::Relaxed)
                        && ui
                            .button("Skip archive check")
                            .on_hover_text("Stop checking the downloaded archives and start installing now.")
                            .clicked()
                    {
                        r.skip_verify.store(true, Ordering::Relaxed);
                    }
                    let cancelled = self.running.as_ref().is_some_and(|r| r.cancel.load(Ordering::Relaxed));
                    ui.spinner();
                    ui.label(if cancelled { "Cancelling…".to_string() } else { self.progress.phase.clone() });
                } else if ui.add_enabled(can_install, egui::Button::new(RichText::new("Install").strong())).clicked() {
                    self.start(&ctx);
                }
            });

            // Live system load: how much of the machine the codecs are using.
            ui.label(
                RichText::new(format!(
                    "CPU {:.0}% (~{:.0} of {} threads busy) · RAM {} / {}",
                    self.cpu * 100.0,
                    self.cpu * self.threads as f64,
                    self.threads,
                    human(self.ram.0),
                    human(self.ram.1)
                ))
                .weak(),
            );
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if busy || !self.progress.jobs.is_empty() {
                    let (done, total): (u64, u64) =
                        self.progress.jobs.values().fold((0, 0), |(d, t), j| (d + j.done.min(j.total), t + j.total));
                    let total = total.max(self.progress.plan_total).max(1);
                    ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(format!(
                        "{}: {} / {}",
                        self.progress.phase,
                        human(done),
                        human(total)
                    )));
                    ui.add_space(4.0);
                    for j in self.progress.jobs.values() {
                        let frac = j.done as f32 / j.total.max(1) as f32;
                        let text = match &j.finished {
                            Some((secs, None, codecs)) => format!("{} done in {secs:.0}s {codecs}", j.label),
                            Some((_, Some(err), _)) => format!("{} FAILED: {}", j.label, err.lines().next().unwrap_or("")),
                            None => {
                                let rate = j.done as f64 / j.started.elapsed().as_secs_f64().max(0.1);
                                format!("{}  {}  {}/s", j.label, j.method, human(rate as u64))
                            }
                        };
                        let bar = egui::ProgressBar::new(frac).text(text);
                        let bar = match &j.finished {
                            Some((_, Some(_), _)) => bar.fill(Color32::DARK_RED),
                            Some(_) => bar.fill(Color32::DARK_GREEN),
                            None => bar,
                        };
                        ui.add(bar);
                    }
                }

                match &self.outcome {
                    Some(Outcome::Done { report, secs }) => {
                        let s = &report.summary;
                        ui.add_space(8.0);
                        let ok = s.problems.is_empty() && !s.cancelled;
                        let title = if s.cancelled {
                            "Cancelled".to_string()
                        } else if ok {
                            format!("Installed {} files ({}) in {:.0} min", s.files_ok + s.files_unverified, human(s.bytes), secs / 60.0)
                        } else {
                            format!("Finished with {} problem(s)", s.problems.len())
                        };
                        ui.heading(title);
                        if report.files_md5 > 0 {
                            ui.label(format!("✔ {} files match the repack's own MD5 list (checked while writing)", report.files_md5));
                        }
                        if report.setup_files > 0 {
                            ui.label(format!("+ {} files from setup.exe itself (launcher, _Redist, MD5 list)", report.setup_files));
                        }
                        for p in &s.problems {
                            ui.colored_label(Color32::LIGHT_RED, p);
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Open game folder").clicked() {
                                let _ = std::process::Command::new("explorer").arg(&self.target).spawn();
                            }
                            if let Some(d) = &report.scripts_dir
                                && ui.button("Open installer scripts").clicked()
                            {
                                let _ = std::process::Command::new("explorer").arg(d).spawn();
                            }
                        });
                        if !report.scripts.is_empty() {
                            ui.label(
                                "The original installer runs these scripts after unpacking. They were not run; \
                                 review them before running them yourself:",
                            );
                            for p in &report.scripts {
                                ui.monospace(p.display().to_string());
                            }
                        }
                    }
                    Some(Outcome::Failed(e)) => {
                        ui.add_space(8.0);
                        ui.colored_label(Color32::LIGHT_RED, format!("Install failed: {e}"));
                    }
                    None => {}
                }

                if !self.progress.log.is_empty() {
                    ui.add_space(8.0);
                    egui::CollapsingHeader::new(format!("Log ({})", self.progress.log.len())).show(ui, |ui| {
                        for (level, text) in &self.progress.log {
                            let color = match level {
                                Level::Info => ui.visuals().text_color(),
                                Level::Warn => Color32::from_rgb(230, 180, 60),
                                Level::Error => Color32::LIGHT_RED,
                            };
                            ui.colored_label(color, text);
                        }
                    });
                }
            });
        });
    }
}
