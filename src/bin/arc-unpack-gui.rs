//! arc-unpack window: pick (or drop) a repack folder, choose what to install, press Install.
//! Launch it by double-clicking, or drop the repack folder onto the .exe.
#![windows_subsystem = "windows"]

use arc_unpack::events::{Event, Level, Sink};
use arc_unpack::extract::human;
use arc_unpack::inno::Setup;
use arc_unpack::install::{self, InstallOptions, InstallReport, Scan};
use arc_unpack::pascal::Script;
use arc_unpack::setup_script::{self as ss, Choice};
use crossbeam_channel::{Receiver, unbounded};
use eframe::egui::{self, Color32, RichText};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const OK: Color32 = Color32::from_rgb(110, 200, 120);
const BAD: Color32 = Color32::from_rgb(240, 110, 110);
const WARN: Color32 = Color32::from_rgb(230, 180, 60);

fn main() -> eframe::Result {
    arc_unpack::guard::kill_descendants_on_exit();
    arc_unpack::perf::tune_self();
    let flags = Flags::parse();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 520.0])
            .with_min_inner_size([560.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("arc-unpack", options, Box::new(move |_cc| Ok(Box::new(App::new(flags)))))
}

/// Command line: an optional repack folder (or a file in it), plus hidden flags used for
/// screenshots and end-to-end tests of the window.
#[derive(Default)]
struct Flags {
    repack: Option<PathBuf>,
    /// `--install-to <dir>`
    target: Option<String>,
    /// `--auto-install`: press Install as soon as it's possible.
    auto_install: bool,
    /// `--screenshot <file.png>`: save the window when it's ready (or, with --auto-install,
    /// `<file>-installing.png` mid-install and `<file>-done.png` at the end), then close.
    screenshot: Option<PathBuf>,
    /// `--select a,b`: tick exactly these components once the repack is read.
    select: Option<BTreeSet<String>>,
}

impl Flags {
    fn parse() -> Flags {
        let mut f = Flags::default();
        let mut args = std::env::args_os().skip(1);
        while let Some(a) = args.next() {
            match a.to_str() {
                Some("--install-to") => f.target = args.next().map(|v| v.to_string_lossy().into_owned()),
                Some("--auto-install") => f.auto_install = true,
                Some("--screenshot") => f.screenshot = args.next().map(PathBuf::from),
                Some("--select") => {
                    f.select = args.next().map(|v| v.to_string_lossy().split(',').map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).collect())
                }
                _ => f.repack = f.repack.take().or_else(|| repack_dir_of(PathBuf::from(&a))),
            }
        }
        f
    }
}

/// A dropped/opened path as a repack folder: the folder itself, or the folder of a setup.exe /
/// .bin file inside it.
fn repack_dir_of(p: PathBuf) -> Option<PathBuf> {
    if p.is_dir() {
        Some(p)
    } else if p.is_file() {
        p.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn default_target(repack: &Path) -> String {
    let name = repack.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // "Title [Some Repack]" -> "Title"
    let clean = name.split(" [").next().unwrap_or(&name).trim().to_string();
    let base = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    format!("{base}\\Games\\{}", if clean.is_empty() { "Game".into() } else { clean })
}

/// "1 min 24 s", "45 s", "1 h 3 min".
fn duration(secs: f64) -> String {
    let s = secs.round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

/// "0:51", "1:02:05".
fn clock(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// "3,489".
fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What reading a repack folder found (done in the background).
struct Analysis {
    repack: PathBuf,
    setup_exe: Option<PathBuf>,
    inno: Option<Arc<Setup>>,
    /// The installer's script, if arc-unpack can run it.
    script: Option<Arc<Script>>,
    scan: Arc<Scan>,
    choices: Vec<Choice>,
    /// The base game's components (unticking all of them = add parts to an installed game).
    base: BTreeSet<String>,
    /// Runtimes the game needs (DirectX, Visual C++, ...), with whether this PC has them.
    runtimes: Vec<arc_unpack::runtimes::Runtime>,
    /// Why the script can't be used (then the archives are extracted directly).
    note: Option<String>,
}

fn analyze(repack: PathBuf) -> Result<Analysis, String> {
    let scan = install::scan(&repack, None).map_err(|e| format!("{e:#}"))?;
    let setup_exe = scan.setup.clone();
    let mut a = Analysis { repack: repack.clone(), setup_exe: setup_exe.clone(), inno: None, script: None, scan: Arc::new(scan), choices: Vec::new(), base: BTreeSet::new(), runtimes: Vec::new(), note: None };
    let Some(exe) = setup_exe else {
        a.note = Some("No setup.exe here, so its install steps can't be run.".into());
        return Ok(a);
    };
    let inno = match Setup::open(&exe) {
        Ok(i) => Arc::new(i),
        Err(e) => {
            a.note = Some(format!("This setup.exe isn't supported ({e:#})."));
            return Ok(a);
        }
    };
    let usable = (|| -> anyhow::Result<(Script, Vec<Choice>)> {
        anyhow::ensure!(!inno.code.is_empty(), "it has no install script");
        let script = Script::load(&inno.code)?;
        let plan = ss::plan_with(&inno, &script, &exe, &repack, Path::new("C:\\arc-unpack-plan"), None)?;
        if let Some(e) = &plan.error {
            anyhow::bail!("{e}");
        }
        let choices = ss::choices(&inno, &plan, &repack);
        Ok((script, choices))
    })();
    match usable {
        Ok((script, choices)) => {
            a.runtimes = arc_unpack::runtimes::of_repack(&inno, Some(&script), &ss::repack_files(&repack));
            a.script = Some(Arc::new(script));
            a.choices = choices;
        }
        Err(e) => a.note = Some(format!("The installer's own steps can't be run ({e:#}); the archives will be extracted directly.")),
    }
    a.base = ss::base_components(&inno);
    a.inno = Some(inno);
    Ok(a)
}

/// "1 step" / "35 steps".
fn steps(n: usize) -> String {
    if n == 1 { "1 step".into() } else { format!("{n} steps") }
}

/// Whether this selection only adds parts to an installed game (no base-game component ticked).
fn adding_parts(a: &Analysis, selected: &BTreeSet<String>) -> bool {
    !a.base.is_empty() && a.base.is_disjoint(selected)
}

/// The ticked optional parts, by their labels ("Bonus Soundtrack, Artbook").
fn part_labels(a: &Analysis, selected: &BTreeSet<String>) -> String {
    let labels: Vec<&str> = a.choices.iter().filter(|c| !a.base.contains(&c.name) && selected.contains(&c.name)).map(|c| c.label.as_str()).collect();
    if labels.is_empty() { "nothing".into() } else { labels.join(", ") }
}

/// What the current selection needs from the folder and the disk.
#[derive(Default, Clone)]
struct Check {
    missing: Vec<String>,
    need: u64,
    steps: usize,
    error: Option<String>,
    /// Archives this selection takes.
    parts: ss::Parts,
    /// Repack archives the selection extracts.
    archives: usize,
    /// Adding parts: the game installed in the chosen folder, or why it isn't usable.
    installed: Option<Result<install::InstalledGame, String>>,
}

fn check(a: &Analysis, selected: &BTreeSet<String>, target: &str) -> Check {
    let (Some(inno), Some(script), Some(exe)) = (&a.inno, &a.script, &a.setup_exe) else {
        let (need, _) = install::needed_size(&a.scan, None);
        return Check { need, ..Default::default() };
    };
    match ss::plan_with(inno, script, exe, &a.repack, Path::new("C:\\arc-unpack-plan"), Some(selected.clone())) {
        Ok(plan) => {
            let pf = ss::preflight(&plan, &a.repack);
            let (need, _) = install::needed_size(&a.scan, Some(&pf));
            let installed = plan.parts.only_optional.then(|| install::find_installed(inno, Path::new(target.trim())).map_err(|e| format!("{e:#}")));
            let archives = pf.needed.len();
            Check { missing: pf.missing, need, steps: plan.steps.len(), error: plan.error, parts: plan.parts, archives, installed }
        }
        Err(e) => Check { error: Some(format!("{e:#}")), ..Default::default() },
    }
}

struct JobView {
    label: String,
    total: u64,
    done: u64,
    /// Decoded bytes expected / produced so far.
    out_total: u64,
    out_done: u64,
    started: Instant,
    finished: Option<(f64, Option<String>)>,
}

#[derive(Default)]
struct Progress {
    phase: String,
    phase_started: Option<Instant>,
    jobs: BTreeMap<usize, JobView>,
    total: u64,
    total_fixed: bool,
    /// Decoded bytes the whole install produces (0 if unknown).
    total_out: u64,
    step: Option<(usize, usize, String)>,
    log: Vec<(Level, String)>,
    /// (time, bytes done) over the last few seconds, for a steady speed readout.
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl Progress {
    /// Bytes per second over the last ~4 seconds.
    fn speed(&mut self, done: u64) -> Option<f64> {
        let now = Instant::now();
        if self.samples.back().is_none_or(|(t, _)| now.duration_since(*t) >= Duration::from_millis(250)) {
            self.samples.push_back((now, done));
        }
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(4)) {
            self.samples.pop_front();
        }
        let (t0, d0) = *self.samples.front()?;
        let secs = now.duration_since(t0).as_secs_f64();
        (secs >= 1.0).then(|| done.saturating_sub(d0) as f64 / secs)
    }
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
    /// The target was filled in automatically (so a new repack may replace it).
    target_auto: bool,
    edited: Instant,
    analysis: Option<Result<Arc<Analysis>, String>>,
    analyzing: Option<(String, Receiver<Result<Analysis, String>>)>,
    selected: BTreeSet<String>,
    check: Option<Check>,
    checking: Option<((BTreeSet<String>, String), Receiver<Check>)>,
    checked_for: Option<(BTreeSet<String>, String)>,
    /// `--select`: the parts to tick once the repack is read.
    preset: Option<BTreeSet<String>>,
    /// The parts being added to an installed game (for the result's title), if that's the run.
    adding: Option<String>,
    /// When the running check started (its result is only dimmed if it takes a moment).
    check_started: Instant,
    limit_ram: bool,
    skip_check: bool,
    /// Install the runtimes this PC is missing after the game (opt-in: it downloads).
    install_runtimes: bool,
    progress: Progress,
    running: Option<Running>,
    outcome: Option<Outcome>,
    shortcut_result: Option<Result<String, String>>,
    cpu: f64,
    ram: (u64, u64),
    free: Option<(String, Option<u64>, Instant)>,
    last_sample: Instant,
    screenshot: Option<PathBuf>,
    /// Frames the window has been settled for (screenshot timing).
    settled_frames: u32,
    shots_taken: u32,
    auto_install: bool,
}

impl App {
    fn new(flags: Flags) -> Self {
        let mut app = App {
            repack: String::new(),
            target: String::new(),
            target_auto: true,
            edited: Instant::now(),
            analysis: None,
            analyzing: None,
            selected: BTreeSet::new(),
            check: None,
            checking: None,
            checked_for: None,
            preset: flags.select,
            adding: None,
            check_started: Instant::now(),
            limit_ram: false,
            skip_check: false,
            install_runtimes: false,
            progress: Progress::default(),
            running: None,
            outcome: None,
            shortcut_result: None,
            cpu: 0.0,
            ram: (0, 0),
            free: None,
            last_sample: Instant::now() - Duration::from_secs(1),
            screenshot: flags.screenshot,
            settled_frames: 0,
            shots_taken: 0,
            auto_install: flags.auto_install,
        };
        if let Some(d) = flags.repack {
            app.set_repack(&d);
        }
        if let Some(t) = flags.target {
            app.target = t;
            app.target_auto = false;
        }
        app
    }

    fn set_repack(&mut self, dir: &Path) {
        self.repack = dir.display().to_string();
        self.edited = Instant::now() - Duration::from_secs(1);
        if self.target_auto || self.target.is_empty() {
            self.target = default_target(dir);
            self.target_auto = true;
        }
    }

    /// Reads the repack folder in the background once its path has stopped changing.
    fn analyze_if_needed(&mut self, ctx: &egui::Context) {
        let path = self.repack.trim().to_string();
        let current = self.analyzing.as_ref().map(|(p, _)| p.clone()).or_else(|| match &self.analysis {
            Some(Ok(a)) => Some(a.repack.display().to_string()),
            _ => None,
        });
        if path.is_empty() || current.as_deref() == Some(path.as_str()) || self.edited.elapsed() < Duration::from_millis(300) {
            return;
        }
        if !Path::new(&path).is_dir() {
            self.analysis = Some(Err("That folder doesn't exist.".into()));
            return;
        }
        let (tx, rx) = unbounded();
        let (p, c) = (PathBuf::from(&path), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(analyze(p));
            c.request_repaint();
        });
        self.analyzing = Some((path, rx));
        self.analysis = None;
        self.check = None;
        self.checked_for = None;
        self.outcome = None;
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        if let Some((path, rx)) = &self.analyzing
            && let Ok(r) = rx.try_recv()
        {
            let stale = path.as_str() != self.repack.trim();
            self.analyzing = None;
            if !stale {
                if let Ok(a) = &r {
                    self.selected = match &self.preset {
                        Some(p) => p.clone(),
                        None => a.choices.iter().filter(|c| c.fixed || c.default).map(|c| c.name.clone()).collect(),
                    };
                }
                self.analysis = Some(r.map(Arc::new));
            }
        }
        let key = (self.selected.clone(), self.target.trim().to_string());
        if let Some((for_key, rx)) = &self.checking
            && let Ok(c) = rx.try_recv()
        {
            if *for_key == key {
                self.check = Some(c);
                self.checked_for = Some(for_key.clone());
            }
            self.checking = None;
        }
        // Re-check what the selection needs whenever it (or the game folder) changes.
        if let Some(Ok(a)) = &self.analysis
            && self.checking.is_none()
            && self.checked_for.as_ref() != Some(&key)
        {
            let (tx, rx) = unbounded();
            let (a, sel, target, c) = (a.clone(), key.0.clone(), key.1.clone(), ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(check(&a, &sel, &target));
                c.request_repaint();
            });
            self.checking = Some((key, rx));
            self.check_started = Instant::now();
        }
    }

    fn free_space(&mut self) -> Option<u64> {
        let stale = self.free.as_ref().is_none_or(|(t, _, at)| *t != self.target || at.elapsed() > Duration::from_secs(3));
        if stale {
            let f = arc_unpack::guard::free_space(Path::new(&self.target));
            self.free = Some((self.target.clone(), f, Instant::now()));
        }
        self.free.as_ref().and_then(|(_, f, _)| *f)
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
        let script = matches!(&self.analysis, Some(Ok(a)) if a.script.is_some());
        self.adding = match (&self.analysis, &self.check) {
            (Some(Ok(a)), Some(c)) if c.parts.only_optional => Some(part_labels(a, &self.selected)),
            _ => None,
        };
        let opts = InstallOptions {
            repack_dir: PathBuf::from(self.repack.trim()),
            target_dir: PathBuf::from(self.target.trim()),
            cache_dir: install::default_cache_dir(),
            config: None,
            verify_md5: !self.skip_check,
            jobs: 3,
            heavy_jobs: if self.limit_ram { 1 } else { 2 },
            writers: 8,
            overwrite: true,
            stall_timeout: 600,
            tune: true,
            host_dir: std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default(),
            cancel: cancel.clone(),
            skip_verify: skip_verify.clone(),
            dry_run: false,
            script,
            components: script.then(|| self.selected.iter().cloned().collect()),
            runtimes: self.install_runtimes && self.adding.is_none(),
        };
        let done_ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = install::install(&opts, &sink);
            let _ = rtx.send(r);
            done_ctx.request_repaint();
        });
        self.progress = Progress::default();
        self.outcome = None;
        self.shortcut_result = None;
        self.running = Some(Running { events: erx, result: rrx, cancel, skip_verify, started: Instant::now() });
    }

    fn pump(&mut self) {
        let Some(run) = &self.running else { return };
        while let Ok(e) = run.events.try_recv() {
            let p = &mut self.progress;
            match e {
                Event::Phase { name } => {
                    p.phase = name;
                    p.phase_started = Some(Instant::now());
                    p.jobs.clear();
                    p.total = 0;
                    p.total_fixed = false;
                    p.total_out = 0;
                    p.samples.clear();
                    p.step = None;
                }
                Event::Total { compressed, output } => {
                    p.total = compressed;
                    p.total_out = output;
                    p.total_fixed = true;
                }
                Event::JobOutput { id, done } => {
                    if let Some(j) = p.jobs.get_mut(&id) {
                        j.out_done = done.max(j.out_done);
                    }
                }
                Event::Plan { compressed, .. } if !p.total_fixed => p.total += compressed,
                Event::Plan { .. } => {}
                Event::Step { index, total, label } => p.step = Some((index, total, label)),
                Event::JobStarted { id, label, compressed, output, .. } => {
                    p.jobs.insert(
                        id,
                        JobView { label, total: compressed, done: 0, out_total: output, out_done: 0, started: Instant::now(), finished: None },
                    );
                }
                Event::JobProgress { id, done } => {
                    if let Some(j) = p.jobs.get_mut(&id) {
                        j.done = done.max(j.done);
                    }
                }
                Event::JobFinished { id, secs, error, .. } => {
                    if let Some(j) = p.jobs.get_mut(&id) {
                        j.done = j.total;
                        j.out_done = j.out_total;
                        j.finished = Some((secs, error));
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

    fn take_dropped(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if self.running.is_none()
            && let Some(dir) = dropped.into_iter().find_map(repack_dir_of)
        {
            self.set_repack(&dir);
        }
    }

    /// Test hooks: --auto-install presses Install once it's possible; --screenshot saves the
    /// window at the right moments and closes it at the end.
    fn test_hooks(&mut self, ctx: &egui::Context) {
        let ready = self.check.as_ref().is_some_and(|c| c.missing.is_empty() && c.error.is_none()) && self.checking.is_none();
        if self.auto_install && ready && self.running.is_none() && self.outcome.is_none() {
            self.auto_install = false;
            self.start(ctx);
        }
        let Some(path) = self.screenshot.clone() else { return };
        let in_install = self.progress.phase == "Installing" && self.progress.step.as_ref().is_some_and(|s| s.0 >= 3);
        let (moment, last) = if self.outcome.is_some() {
            (Some("done"), true)
        } else if in_install && self.shots_taken == 0 && self.running.is_some() {
            (Some("installing"), false)
        } else if !self.auto_install && self.running.is_none() && self.outcome.is_none() && ready && self.analyzing.is_none() {
            (Some(""), true)
        } else {
            (None, false)
        };
        let Some(moment) = moment else {
            self.settled_frames = 0;
            ctx.request_repaint();
            return;
        };
        self.settled_frames += 1;
        if self.settled_frames == 20 {
            let file = if moment.is_empty() {
                path
            } else {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                path.with_file_name(format!("{stem}-{moment}.png"))
            };
            self.shots_taken += 1;
            let c = ctx.clone();
            ctx.request_screenshot(move |img| {
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
                let _ = save_png(&file, img.size[0] as u32, img.size[1] as u32, &rgba);
                if last {
                    c.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                c.request_repaint();
            });
        }
        ctx.request_repaint();
    }
}

fn save_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

fn folder_row(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, enabled: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([72.0, 22.0], egui::Label::new(label));
        let w = ui.available_width() - 84.0;
        let edit = ui.add_enabled(enabled, egui::TextEdit::singleline(value).hint_text(hint).desired_width(w));
        changed |= edit.changed();
        if ui.add_enabled(enabled, egui::Button::new("Browse…")).clicked()
            && let Some(d) = rfd::FileDialog::new().pick_folder()
        {
            *value = d.display().to_string();
            changed = true;
        }
    });
    changed
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        self.take_dropped(&ctx);
        self.analyze_if_needed(&ctx);
        self.poll_background(&ctx);
        self.sample_system();
        if self.running.is_some() || self.analyzing.is_some() || self.checking.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let busy = self.running.is_some();
        let dragging = ctx.input(|i| !i.raw.hovered_files.is_empty());

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.heading("arc-unpack");
            ui.label(RichText::new("Installs repacks faster, using the installer's own steps").weak());
            ui.add_space(10.0);

            let before = self.repack.clone();
            if folder_row(ui, "Repack", &mut self.repack, "the repack folder (with setup.exe and the .bin files)", !busy) && self.repack != before {
                self.edited = Instant::now();
                if self.target_auto {
                    self.target = default_target(Path::new(self.repack.trim()));
                }
            }
            if folder_row(ui, "Install to", &mut self.target, "where the game goes", !busy) {
                self.target_auto = false;
            }
            ui.add_space(8.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if dragging {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| ui.label(RichText::new("Drop to use this repack").size(18.0)));
                    return;
                }
                self.before_install(ui, &ctx, busy);
                self.installing(ui, busy);
                self.result(ui);
                if !self.progress.log.is_empty() {
                    ui.add_space(10.0);
                    egui::CollapsingHeader::new(format!("Log ({})", self.progress.log.len())).show(ui, |ui| {
                        for (level, text) in &self.progress.log {
                            let color = match level {
                                Level::Info => ui.visuals().text_color(),
                                Level::Warn => WARN,
                                Level::Error => BAD,
                            };
                            ui.colored_label(color, text);
                        }
                    });
                }
            });
        });
        self.test_hooks(&ctx);
    }
}

impl App {
    fn before_install(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, busy: bool) {
        if self.repack.trim().is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Drop a repack folder here, or press Browse").size(16.0));
                ui.label(RichText::new("The folder with setup.exe and the fg-*.bin files").weak());
            });
            return;
        }
        if self.analyzing.is_some() || (self.analysis.is_none() && Path::new(self.repack.trim()).is_dir()) {
            let h = ui.text_style_height(&egui::TextStyle::Body);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(h));
                ui.label(RichText::new("Reading setup.exe…").weak());
            });
            return;
        }
        let a = match &self.analysis {
            Some(Ok(a)) => a.clone(),
            Some(Err(e)) => {
                ui.colored_label(BAD, e);
                return;
            }
            None => return,
        };

        // What to install.
        if !a.choices.is_empty() {
            ui.label(RichText::new("What to install").strong());
            let mut toggled: Option<(String, bool, bool)> = None;
            let adding = adding_parts(&a, &self.selected);
            let installed_parts = match &self.check {
                Some(Check { installed: Some(Ok(g)), .. }) if adding => g.parts.clone(),
                _ => BTreeSet::new(),
            };
            for c in &a.choices {
                ui.horizontal(|ui| {
                    let mut on = self.selected.contains(&c.name);
                    let mut resp = ui.add_enabled(!busy, egui::Checkbox::new(&mut on, &c.label));
                    if a.base.contains(&c.name) {
                        resp = resp.on_hover_text("Untick to add only the parts below to a game that's already installed");
                    }
                    if resp.changed() {
                        toggled = Some((c.name.clone(), on, c.exclusive));
                    }
                    if a.base.contains(&c.name) {
                        ui.label(RichText::new(if adding { "not reinstalled: adding parts to the installed game" } else { "base game" }).weak());
                    } else if installed_parts.contains(&c.name) {
                        ui.label(RichText::new("already installed").weak());
                    } else if let Some(f) = &c.file {
                        ui.label(RichText::new(format!("downloaded · {f}")).weak());
                    }
                });
            }
            if let Some((name, on, exclusive)) = toggled {
                if on {
                    if exclusive {
                        // Only one of an exclusive group (e.g. a language) at a time.
                        let parent = |n: &str| n.rsplit_once('\\').map(|(p, _)| p.to_string()).unwrap_or_default();
                        let group = parent(&name);
                        for other in a.choices.iter().filter(|o| o.exclusive && o.name != name && parent(&o.name) == group) {
                            self.selected.remove(&other.name);
                        }
                    }
                    self.selected.insert(name);
                } else {
                    self.selected.remove(&name);
                }
            }
            ui.add_space(6.0);
        }

        // Pre-flight: are the needed files here, and does it fit? While a changed selection is
        // re-checked, the previous result stays in place (dimmed if it takes a moment), so
        // nothing below moves.
        let free = self.free_space();
        let mut ready = false;
        let line_h = ui.text_style_height(&egui::TextStyle::Body);
        let dim = self.checking.is_some() && self.check_started.elapsed() > Duration::from_millis(150);
        let tint = |c: Color32, ui: &egui::Ui| if dim { ui.visuals().weak_text_color() } else { c };
        match &self.check {
            None => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(line_h));
                    ui.label(RichText::new("Checking what's needed…").weak());
                });
            }
            Some(c) => {
                let drive = Path::new(self.target.trim())
                    .components()
                    .next()
                    .map(|p| p.as_os_str().to_string_lossy().into_owned())
                    .unwrap_or_default();
                let fits = free.is_none_or(|f| f > c.need + (1 << 30));
                let adding = c.parts.only_optional;
                if let (true, Some(Err(e))) = (adding, &c.installed) {
                    ui.colored_label(tint(BAD, ui), format!("{e}. Choose the folder the game is installed in, or tick the base game."));
                } else if adding && c.archives == 0 {
                    ui.colored_label(tint(WARN, ui), "Tick the parts to add to the installed game.");
                } else if !c.missing.is_empty() {
                    let (s, it) = if c.missing.len() == 1 { ("", "it") } else { ("s", "them") };
                    ui.colored_label(
                        tint(BAD, ui),
                        format!("Missing file{s}: {}. Download {it} into the repack folder, or untick the part that needs {it}.", c.missing.join(", ")),
                    );
                } else if let Some(e) = &c.error {
                    ui.colored_label(tint(BAD, ui), format!("The installer's steps can't be run: {e}"));
                } else if !fits {
                    ui.colored_label(
                        tint(BAD, ui),
                        format!("Needs {}, but only {} is free on {drive}", human(c.need), human(free.unwrap_or(0))),
                    );
                } else {
                    let mut line = if adding {
                        format!("Game found · adds {} · needs {}", part_labels(&a, &self.selected), human(c.need))
                    } else {
                        format!("All needed files are here · needs {}", human(c.need))
                    };
                    if let Some(f) = free {
                        line.push_str(&format!(" · {} free on {drive}", human(f)));
                    }
                    ui.colored_label(tint(OK, ui), line);
                    // Pressing Install mid-check is safe: the install checks for itself.
                    ready = true;
                }
            }
        }
        if let Some(note) = &a.note {
            ui.colored_label(WARN, note);
        }

        // Options, in FitGirl's words.
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_enabled(!busy, egui::Checkbox::new(&mut self.limit_ram, "Limit RAM usage"))
                .on_hover_text("For PCs with 16 GB of RAM or less: runs one memory-hungry decoder at a time. Slower.");
            ui.add_space(12.0);
            let has_list = a.scan.md5_file.is_some();
            ui.add_enabled(!busy && has_list, egui::Checkbox::new(&mut self.skip_check, "Skip archive check")).on_hover_text(if has_list {
                "Don't check the downloaded .bin files before installing. Faster; installed files are still checked as they're written."
            } else {
                "This repack has no MD5 list for its .bin files."
            });
        });
        // Only offered when this PC lacks something the game needs (most PCs have them all).
        let missing = arc_unpack::runtimes::missing(&a.runtimes);
        if !missing.is_empty() && !adding_parts(&a, &self.selected) {
            let names = arc_unpack::runtimes::labels(&missing);
            ui.add_enabled(!busy, egui::Checkbox::new(&mut self.install_runtimes, format!("Also install missing runtimes: {names}")))
                .on_hover_text(
                    "This PC doesn't have them, and the game may not start without them. They're downloaded from Microsoft \
                     (DirectX's installer comes with the repack), checked for Microsoft's signature, and installed silently \
                     after the game. FitGirl's installer offers the same.",
                );
        }

        // The one primary action (gone once this install succeeded; picking another folder
        // brings it back).
        ui.add_space(10.0);
        let installed = matches!(&self.outcome, Some(Outcome::Done { report, .. }) if report.summary.problems.is_empty() && !report.summary.cancelled);
        if !busy && !installed {
            let can = ready && !self.target.trim().is_empty();
            let adding = matches!(&self.check, Some(c) if c.parts.only_optional);
            let button = egui::Button::new(RichText::new(if adding { "Add to installed game" } else { "Install" }).size(18.0).strong())
                .fill(if can { ui.visuals().selection.bg_fill } else { ui.visuals().widgets.inactive.bg_fill });
            let clicked = ui
                .with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
                    ui.add_enabled(can, button.min_size(egui::vec2(0.0, 40.0))).clicked()
                })
                .inner;
            if clicked {
                self.start(ctx);
            }
            if let Some(c) = &self.check
                && c.steps > 0
                && can
            {
                let line = if c.parts.only_optional {
                    format!("Runs {} in {}", steps(c.steps), self.target.trim())
                } else {
                    format!("Runs the installer's {} into {}", steps(c.steps), self.target.trim())
                };
                ui.label(RichText::new(line).weak());
            }
        }
    }

    fn installing(&mut self, ui: &mut egui::Ui, busy: bool) {
        if !busy {
            return;
        }
        // Measured in decoded output when it's known (decoders may read all their input first);
        // otherwise in compressed input.
        let p = &self.progress;
        let out_done: u64 = p.jobs.values().map(|j| j.out_done.min(j.out_total.max(j.out_done))).sum();
        let by_output = p.total_out > 0 || p.jobs.values().any(|j| j.out_done > 0);
        let (done, total) = if by_output {
            let total: u64 = if p.total_out > 0 { p.total_out } else { p.jobs.values().map(|j| j.out_total).sum() };
            (out_done, total)
        } else {
            let (d, t) = p.jobs.values().fold((0u64, 0u64), |(d, t), j| (d + j.done.min(j.total), t + j.total));
            (d, if p.total_fixed { p.total } else { t.max(p.total) })
        };
        let speed = self.progress.speed(done);
        let p = &self.progress;
        let total = total.max(1);
        let frac = (done as f64 / total as f64).min(0.995);
        let elapsed = p.phase_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let cancelled = self.running.as_ref().is_some_and(|r| r.cancel.load(Ordering::Relaxed));
        let (title, sub) = match p.phase.as_str() {
            _ if cancelled => ("Cancelling…".to_string(), String::new()),
            "Verifying archives" => ("Checking the downloaded files".to_string(), String::new()),
            "Checking files" => ("Checking the installed files".to_string(), String::new()),
            "Installing" | "Installing game files" => (
                "Installing".to_string(),
                p.step.as_ref().map(|(i, n, l)| format!("Step {i} of {n} · {l}")).unwrap_or_default(),
            ),
            other => (other.to_string(), String::new()),
        };
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(&title).strong().size(16.0));
        });
        if !sub.is_empty() {
            ui.label(&sub);
        }
        let mut text = format!("{}%", (frac * 100.0).floor());
        if let Some(bps) = speed.filter(|b| *b > 0.0) {
            text.push_str(&format!(" · {}/s", human(bps as u64)));
        }
        text.push_str(&format!(" · {} elapsed", clock(elapsed)));
        if frac > 0.02 && elapsed > 5.0 && !p.jobs.is_empty() {
            text.push_str(&format!(" · about {} left", clock(elapsed * (1.0 - frac) / frac)));
        }
        let show_bar = !p.jobs.is_empty() || p.total_fixed;
        if show_bar {
            ui.add(egui::ProgressBar::new(frac as f32).text(text));
        }
        ui.horizontal(|ui| {
            if let Some(r) = &self.running {
                if ui.button("Cancel").clicked() {
                    r.cancel.store(true, Ordering::Relaxed);
                }
                if p.phase == "Verifying archives"
                    && !r.skip_verify.load(Ordering::Relaxed)
                    && ui.button("Skip archive check").on_hover_text("Stop checking the downloaded files and start installing now.").clicked()
                {
                    r.skip_verify.store(true, Ordering::Relaxed);
                }
            }
            ui.label(
                RichText::new(format!("CPU {:.0}% · RAM {} of {}", self.cpu * 100.0, human(self.ram.0), human(self.ram.1))).weak(),
            );
        });
        if !p.jobs.is_empty() {
            egui::CollapsingHeader::new("Details").default_open(false).show(ui, |ui| {
                let running = p.jobs.values().filter(|j| j.finished.is_none());
                let finished: Vec<&JobView> = p.jobs.values().filter(|j| j.finished.is_some()).collect();
                for j in running {
                    let (d, t) = if j.out_total > 0 && j.out_done > 0 { (j.out_done, j.out_total) } else { (j.done, j.total) };
                    let rate = d as f64 / j.started.elapsed().as_secs_f64().max(0.1);
                    ui.add(egui::ProgressBar::new(d as f32 / t.max(1) as f32).text(format!("{} · {}/s", j.label, human(rate as u64))));
                }
                for j in finished.iter().rev().take(5) {
                    match &j.finished {
                        Some((secs, None)) => ui.label(RichText::new(format!("{} done ({})", j.label, duration(*secs))).weak()),
                        Some((_, Some(err))) => ui.colored_label(BAD, format!("{} failed: {}", j.label, err.lines().next().unwrap_or(""))),
                        None => unreachable!(),
                    };
                }
                if finished.len() > 5 {
                    ui.label(RichText::new(format!("… and {} more finished", finished.len() - 5)).weak());
                }
            });
        }
    }

    fn result(&mut self, ui: &mut egui::Ui) {
        let Some(outcome) = &self.outcome else { return };
        ui.add_space(12.0);
        match outcome {
            Outcome::Done { report, secs } => {
                let s = &report.summary;
                let ok = s.problems.is_empty() && !s.cancelled;
                let (title, color) = if s.cancelled {
                    ("Cancelled".to_string(), WARN)
                } else if ok {
                    (self.adding.as_ref().map(|p| format!("Added {p}")).unwrap_or_else(|| "Installed".to_string()), OK)
                } else {
                    (format!("Finished with {} problem{}", s.problems.len(), if s.problems.len() == 1 { "" } else { "s" }), BAD)
                };
                ui.label(RichText::new(title).size(20.0).strong().color(color));
                if !s.cancelled {
                    ui.label(format!(
                        "{} files · {} · {} · {}/s on average",
                        count(s.files_ok + s.files_unverified),
                        human(s.bytes),
                        duration(*secs),
                        human((s.bytes as f64 / secs.max(0.1)) as u64)
                    ));
                }
                if report.files_md5 > 0 {
                    ui.colored_label(OK, format!("All {} files match the repack's MD5 list", count(report.files_md5 as u64)));
                }
                for p in &s.problems {
                    ui.colored_label(BAD, p);
                }
                for n in s.notes.iter().filter(|n| n.contains(install::BACKUP_DIR)) {
                    ui.label(RichText::new(n).weak());
                }
                for line in &report.runtimes {
                    ui.colored_label(OK, line);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Open game folder").clicked() {
                        let _ = std::process::Command::new("explorer").arg(self.target.trim()).spawn();
                    }
                    if ok && !report.shortcuts.is_empty() && self.shortcut_result.is_none() {
                        let label = if report.shortcuts.len() == 1 { "Create desktop shortcut" } else { "Create desktop shortcuts" };
                        if ui.button(label).clicked() {
                            self.shortcut_result = Some(match ss::create_desktop_shortcuts(&report.shortcuts) {
                                Ok(made) => Ok(format!("Created {} on the desktop", made.iter().filter_map(|p| p.file_stem()).map(|n| n.to_string_lossy()).collect::<Vec<_>>().join(", "))),
                                Err(e) => Err(format!("{e:#}")),
                            });
                        }
                    }
                });
                match &self.shortcut_result {
                    Some(Ok(t)) => {
                        ui.colored_label(OK, t);
                    }
                    Some(Err(e)) => {
                        ui.colored_label(BAD, format!("Couldn't create the shortcut: {e}"));
                    }
                    None => {}
                }
            }
            Outcome::Failed(e) => {
                ui.label(RichText::new("Install failed").size(20.0).strong().color(BAD));
                ui.colored_label(BAD, e);
            }
        }
    }
}
