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

// White on black with one accent (docs/logo.svg).
const BG: Color32 = Color32::from_rgb(10, 10, 10);
const SURFACE: Color32 = Color32::from_rgb(20, 20, 22);
const BORDER: Color32 = Color32::from_rgb(39, 39, 42);
const BORDER_STRONG: Color32 = Color32::from_rgb(63, 63, 70);
const TEXT: Color32 = Color32::from_rgb(228, 228, 231);
const MUTED: Color32 = Color32::from_rgb(161, 161, 170);
const ACCENT: Color32 = Color32::from_rgb(34, 197, 94);
const OK: Color32 = Color32::from_rgb(74, 222, 128);
const BAD: Color32 = Color32::from_rgb(248, 113, 113);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);

/// The mark, 128 x 128 RGBA (made from docs/logo.svg).
const LOGO: &[u8] = include_bytes!("../../assets/icon-128.rgba");

fn semibold() -> egui::FontFamily {
    egui::FontFamily::Name("semibold".into())
}

/// Semibold text of a given size (headings, the primary button).
fn sb(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).font(egui::FontId::new(size, semibold()))
}

/// Fonts and colours. Segoe UI comes from this PC's Windows folder; without it egui's own
/// font is used.
fn apply_theme(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into())).join("Fonts");
    if let Ok(bytes) = std::fs::read(dir.join("segoeui.ttf")) {
        fonts.font_data.insert("segoe".into(), Arc::new(egui::FontData::from_owned(bytes)));
        fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    let mut bold: Vec<String> = Vec::new();
    if let Ok(bytes) = std::fs::read(dir.join("seguisb.ttf")) {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(egui::FontData::from_owned(bytes)));
        bold.push("segoe-semibold".into());
    }
    bold.extend(fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(semibold(), bold);
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        use egui::{FontFamily::Proportional, FontId, Stroke, TextStyle};
        style.text_styles.insert(TextStyle::Body, FontId::new(15.0, Proportional));
        style.text_styles.insert(TextStyle::Button, FontId::new(15.0, Proportional));
        style.text_styles.insert(TextStyle::Small, FontId::new(12.5, Proportional));
        style.text_styles.insert(TextStyle::Heading, FontId::new(22.0, semibold()));
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style.spacing.interact_size.y = 32.0;
        style.spacing.icon_width = 18.0;
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = BG;
        v.extreme_bg_color = SURFACE;
        v.faint_bg_color = SURFACE;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = Color32::from_rgb(22, 101, 52);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        let radius = egui::CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.noninteractive.corner_radius = radius;
        for (state, fill, border) in [
            (&mut w.inactive, Color32::from_rgb(32, 32, 36), BORDER),
            (&mut w.hovered, Color32::from_rgb(44, 44, 48), BORDER_STRONG),
            (&mut w.active, Color32::from_rgb(56, 56, 62), BORDER_STRONG),
            (&mut w.open, Color32::from_rgb(44, 44, 48), BORDER_STRONG),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::new(1.0, border);
            state.fg_stroke = Stroke::new(1.0, TEXT);
            state.corner_radius = radius;
            state.expansion = 0.0;
        }
    });
}

/// A status line: a small dot in `color`, then the text (red when it's an error, dimmed when
/// the line is being re-checked).
fn status(ui: &mut egui::Ui, color: Color32, text: impl Into<String>) {
    ui.horizontal_wrapped(|ui| {
        let h = ui.text_style_height(&egui::TextStyle::Body);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, h), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        let ink = if color == BAD {
            BAD
        } else if color == ui.visuals().weak_text_color() {
            color
        } else {
            ui.visuals().text_color()
        };
        ui.label(RichText::new(text).color(ink));
    });
}

/// A rounded progress bar.
fn bar(ui: &mut egui::Ui, fraction: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let r = height / 2.0;
    ui.painter().rect_filled(rect, r, BORDER);
    let mut fill = rect;
    fill.set_width((rect.width() * fraction.clamp(0.0, 1.0)).max(height));
    ui.painter().rect_filled(fill, r, ACCENT);
}

/// A bordered group for one section of the window.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The drop area shown until a repack is picked, in the logo's style (as docs/drop.svg): its three
/// streams, white, green and white, trail a folder that the pointer carries in. While a folder is
/// dragged over the window, the area turns green and the folder settles. (Run as administrator,
/// the window only hears of the drop itself, not the drag before it, so then nothing lights up.)
fn drop_zone(ui: &mut egui::Ui, active: bool) -> egui::Response {
    use egui::epaint::RectShape;
    use egui::{CornerRadius, Rect, Shape, Stroke, pos2, vec2};
    let ctx = ui.ctx().clone();
    let t = ctx.animate_bool_with_time(egui::Id::unique("drop-zone-active"), active, 0.2);
    // The art is drawn in the units of docs/drop.svg (x 180..640, y 120..350), at this scale;
    // below it, two lines of text.
    let s = 0.55;
    let (pad, text_h) = (30.0, 16.0 + 29.0 + 20.0);
    let (zone, response) = ui.allocate_exact_size(vec2(ui.available_width(), pad + 230.0 * s + text_h + pad), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    // Clicking it opens the folder picker: a lighter border says so on hover.
    let border = if response.hovered() && !active { MUTED.gamma_multiply(0.7) } else { BORDER_STRONG };
    let painter = ui.painter_at(zone);
    painter.rect(
        zone,
        12u8,
        Color32::TRANSPARENT.lerp_to_gamma(ACCENT.gamma_multiply(0.08), t),
        Stroke::new(1.0 + t, border.lerp_to_gamma(ACCENT, t)),
        egui::StrokeKind::Inside,
    );

    let origin = pos2(zone.center().x - (640.0 - 180.0) * s / 2.0, zone.top() + pad);
    let at = |x: f32, y: f32| origin + vec2((x - 180.0) * s, (y - 120.0) * s);

    // The streams: fading in from the left, as long as the logo's (green longest, then white,
    // white), ending under the folder.
    let ramp_id = egui::Id::unique("drop-zone-ramp");
    let ramp = ctx.data(|d| d.get_temp::<egui::TextureHandle>(ramp_id)).unwrap_or_else(|| {
        let pixels = (0..64).map(|i| Color32::from_white_alpha((i * 255 / 63) as u8)).collect();
        let texture = ctx.load_texture("drop-zone-ramp", egui::ColorImage::new([64, 1], pixels), egui::TextureOptions::LINEAR);
        ctx.data_mut(|d| d.insert_temp(ramp_id, texture.clone()));
        texture
    });
    let up = vec2(2.0, -1.0).normalized();
    let white = Color32::WHITE.gamma_multiply(0.3);
    for (y, len, color) in [(180.0, 230.0, white), (215.0, 280.0, ACCENT.gamma_multiply(0.95)), (250.0, 170.0, white)] {
        let center = at(470.0, y) - up * (len * s / 2.0);
        let stream = RectShape::filled(Rect::from_center_size(center, vec2(len * s, 16.0 * s)), 0u8, color)
            .with_texture(ramp.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)))
            .with_angle(up.angle());
        painter.add(stream);
    }

    // The folder, tilted on its way in; it straightens and settles while a drop is coming.
    let shift = vec2(4.0, 12.0) * (t * s);
    let tilt = (-8.0f32).to_radians() * (1.0 - t);
    let pivot = at(540.0, 200.0) + shift;
    let r = |v: f32| (v * s).round() as u8;
    let part = |x0: f32, y0: f32, x1: f32, y1: f32, corners: CornerRadius, fill: Color32| {
        RectShape::filled(Rect::from_min_max(at(x0, y0) + shift, at(x1, y1) + shift), corners, fill).with_angle_and_pivot(tilt, pivot)
    };
    painter.add(part(460.0, 138.0, 530.0, 168.0, CornerRadius { nw: r(14.0), ne: r(8.0), sw: 0, se: 0 }, MUTED));
    painter.add(part(460.0, 154.0, 620.0, 262.0, CornerRadius::same(r(14.0)), MUTED));
    painter.add(part(460.0, 170.0, 620.0, 262.0, CornerRadius { nw: r(12.0), ne: r(12.0), sw: r(14.0), se: r(14.0) }, Color32::WHITE));

    // The pointer carrying it: filled as triangles (its outline isn't convex), then outlined.
    let tip = at(598.0, 224.0) + shift;
    let outline: Vec<egui::Pos2> =
        [(0.0, 0.0), (0.0, 40.0), (10.0, 31.0), (18.0, 48.0), (26.0, 44.0), (18.0, 28.0), (32.0, 28.0)].iter().map(|&(x, y)| tip + vec2(x, y) * s).collect();
    let mut mesh = egui::Mesh::default();
    for &p in &outline {
        mesh.colored_vertex(p, Color32::WHITE);
    }
    for [a, b, c] in [[0, 1, 2], [0, 2, 5], [0, 5, 6], [2, 3, 4], [2, 4, 5]] {
        mesh.add_triangle(a, b, c);
    }
    painter.add(Shape::mesh(mesh));
    painter.add(Shape::closed_line(outline, Stroke::new(1.4, BG)));

    let title = if active { "Drop to use this repack" } else { "Drop a repack folder here, or click to browse" };
    let y = at(0.0, 350.0).y + 16.0;
    painter.text(pos2(zone.center().x, y), egui::Align2::CENTER_TOP, title, egui::FontId::new(17.0, semibold()), TEXT.lerp_to_gamma(Color32::WHITE, t));
    painter.text(
        pos2(zone.center().x, y + 29.0),
        egui::Align2::CENTER_TOP,
        "The folder with setup.exe and the fg-*.bin files",
        egui::TextStyle::Body.resolve(ui.style()),
        MUTED,
    );
    response
}

fn main() -> eframe::Result {
    arc_unpack::guard::kill_descendants_on_exit();
    arc_unpack::perf::tune_self();
    let flags = Flags::parse();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 600.0])
            .with_min_inner_size([600.0, 460.0])
            .with_icon(egui::IconData { rgba: LOGO.to_vec(), width: 128, height: 128 })
            // Run as administrator, the window can't receive Explorer's drag-and-drop: see
            // `elevated_drop`, which takes over then.
            .with_drag_and_drop(!arc_unpack::guard::is_elevated()),
        ..Default::default()
    };
    eframe::run_native(
        "arc-unpack",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            if arc_unpack::guard::is_elevated() {
                elevated_drop::install(cc);
            }
            Ok(Box::new(App::new(flags)))
        }),
    )
}

/// Drag-and-drop for a window run as administrator. Explorer runs without administrator rights,
/// and Windows doesn't deliver its (OLE) drag-and-drop to a program with more rights. The older
/// WM_DROPFILES way can be let through, so the window uses that instead.
mod elevated_drop {
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, DragAcceptFiles, DragFinish, DragQueryFileW, HDROP, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ChangeWindowMessageFilterEx, MSGFLT_ALLOW, WM_COPYDATA, WM_DROPFILES};

    /// The message Windows uses to copy the dropped file list across to the window.
    const WM_COPYGLOBALDATA: u32 = 0x0049;
    static DROPPED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static CTX: OnceLock<eframe::egui::Context> = OnceLock::new();

    pub fn install(cc: &eframe::CreationContext<'_>) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = cc.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
        let hwnd = h.hwnd.get() as HWND;
        let _ = CTX.set(cc.egui_ctx.clone());
        // SAFETY: a live window of this thread; the subclass procedure lives for the program.
        unsafe {
            for msg in [WM_DROPFILES, WM_COPYDATA, WM_COPYGLOBALDATA] {
                ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, std::ptr::null_mut());
            }
            DragAcceptFiles(hwnd, 1);
            SetWindowSubclass(hwnd, Some(on_message), 1, 0);
        }
    }

    /// Paths dropped since the last call.
    pub fn take() -> Vec<PathBuf> {
        std::mem::take(&mut *DROPPED.lock().unwrap_or_else(|e| e.into_inner()))
    }

    unsafe extern "system" fn on_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _: usize, _: usize) -> LRESULT {
        if msg != WM_DROPFILES {
            return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        }
        let drop = wparam as HDROP;
        let mut paths = Vec::new();
        // SAFETY: `drop` is the HDROP Windows passed with WM_DROPFILES; buffers are sized to it.
        unsafe {
            for i in 0..DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0) {
                let mut buf = vec![0u16; DragQueryFileW(drop, i, std::ptr::null_mut(), 0) as usize + 1];
                let n = DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32) as usize;
                paths.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])));
            }
            DragFinish(drop);
        }
        DROPPED.lock().unwrap_or_else(|e| e.into_inner()).extend(paths);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        0
    }
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
    /// `--ram-limit <GB>`: tick "Limit RAM to" with this value.
    ram_limit: Option<u32>,
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
                Some("--ram-limit") => f.ram_limit = args.next().and_then(|v| v.to_str()?.parse().ok()),
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
    /// Optional parts that weren't downloaded, with their missing archives.
    not_downloaded: BTreeMap<String, Vec<String>>,
}

/// The optional parts whose archives aren't in the repack folder, with those archives (the
/// repack's own installer greys these out too). Each part is planned on its own on top of the
/// base game: the optional archives it adds that are missing are its own.
fn not_downloaded(inno: &Setup, script: &Script, exe: &Path, repack: &Path, choices: &[Choice], base: &BTreeSet<String>) -> BTreeMap<String, Vec<String>> {
    let missing = |selected: BTreeSet<String>| -> Vec<String> {
        match ss::plan_with(inno, script, exe, repack, Path::new("C:\\arc-unpack-plan"), Some(selected)) {
            Ok(plan) => ss::preflight(&plan, repack).missing.into_iter().filter(|m| ss::is_optional_archive(m)).collect(),
            Err(_) => Vec::new(),
        }
    };
    let always: BTreeSet<String> = choices.iter().filter(|c| c.fixed || base.contains(&c.name)).map(|c| c.name.clone()).collect();
    let before = missing(always.clone());
    let mut out = BTreeMap::new();
    for c in choices.iter().filter(|c| !always.contains(&c.name)) {
        let mut selected = always.clone();
        selected.insert(c.name.clone());
        if let Some((parent, _)) = c.name.rsplit_once('\\') {
            selected.insert(parent.to_string());
        }
        let own: Vec<String> = missing(selected).into_iter().filter(|m| !before.contains(m)).collect();
        if !own.is_empty() {
            out.insert(c.name.clone(), own);
        }
    }
    out
}

fn analyze(repack: PathBuf) -> Result<Analysis, String> {
    let scan = install::scan(&repack, None).map_err(|e| format!("{e:#}"))?;
    let setup_exe = scan.setup.clone();
    let mut a = Analysis {
        repack: repack.clone(),
        setup_exe: setup_exe.clone(),
        inno: None,
        script: None,
        scan: Arc::new(scan),
        choices: Vec::new(),
        base: BTreeSet::new(),
        runtimes: Vec::new(),
        note: None,
        not_downloaded: BTreeMap::new(),
    };
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
            a.base = ss::base_components(&inno);
            a.not_downloaded = not_downloaded(&inno, &script, &exe, &repack, &choices, &a.base);
            a.script = Some(Arc::new(script));
            a.choices = choices;
        }
        Err(e) => {
            a.note = Some(format!("The installer's own steps can't be run ({e:#}); the archives will be extracted directly."));
            a.base = ss::base_components(&inno);
        }
    }
    a.inno = Some(inno);
    Ok(a)
}

/// Creates the installer's desktop shortcuts: "Created <names> on the desktop", or why not.
fn make_shortcuts(shortcuts: &[ss::Shortcut]) -> Result<String, String> {
    match ss::create_desktop_shortcuts(shortcuts) {
        Ok(made) => Ok(format!("Created {} on the desktop", made.iter().filter_map(|p| p.file_stem()).map(|n| n.to_string_lossy()).collect::<Vec<_>>().join(", "))),
        Err(e) => Err(format!("{e:#}")),
    }
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
    /// The folder `analysis` is about (also when reading it failed, so it isn't read again and again).
    analyzed: Option<String>,
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
    /// The limit to use when `limit_ram` is ticked, in GB.
    ram_limit_gb: u32,
    skip_check: bool,
    /// Install the runtimes this PC is missing after the game (opt-in: it downloads).
    install_runtimes: bool,
    progress: Progress,
    running: Option<Running>,
    outcome: Option<Outcome>,
    /// "Create a desktop shortcut" (made once the install succeeded).
    create_shortcut: bool,
    shortcut_result: Option<Result<String, String>>,
    cpu: f64,
    ram: (u64, u64),
    free: Option<(String, Option<u64>, Instant)>,
    last_sample: Instant,
    logo: Option<egui::TextureHandle>,
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
            analyzed: None,
            analyzing: None,
            selected: BTreeSet::new(),
            check: None,
            checking: None,
            checked_for: None,
            preset: flags.select,
            adding: None,
            check_started: Instant::now(),
            limit_ram: flags.ram_limit.is_some(),
            ram_limit_gb: flags.ram_limit.map_or_else(default_ram_limit_gb, |gb| gb.clamp(2, physical_ram_gb())),
            skip_check: false,
            // As FitGirl's installer, which ticks DirectX and Visual C++ (only offered when missing).
            install_runtimes: true,
            progress: Progress::default(),
            running: None,
            outcome: None,
            create_shortcut: true,
            shortcut_result: None,
            cpu: 0.0,
            ram: (0, 0),
            free: None,
            last_sample: Instant::now() - Duration::from_secs(1),
            logo: None,
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
        let current = self.analyzing.as_ref().map(|(p, _)| p.clone()).or_else(|| self.analyzed.clone());
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
            let path = path.clone();
            let stale = path.as_str() != self.repack.trim();
            self.analyzing = None;
            if !stale {
                if let Ok(a) = &r {
                    self.selected = match &self.preset {
                        Some(p) => p.clone(),
                        None => a
                            .choices
                            .iter()
                            .filter(|c| (c.fixed || c.default) && !a.not_downloaded.contains_key(&c.name))
                            .map(|c| c.name.clone())
                            .collect(),
                    };
                }
                self.analysis = Some(r.map(Arc::new));
                self.analyzed = Some(path.clone());
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
            memory_limit: self.limit_ram.then(|| (self.ram_limit_gb.max(1) as u64) << 30),
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
            if let Some(Outcome::Done { report, .. }) = &self.outcome
                && self.create_shortcut
                && self.adding.is_none()
                && report.summary.problems.is_empty()
                && !report.summary.cancelled
                && !report.shortcuts.is_empty()
            {
                self.shortcut_result = Some(make_shortcuts(&report.shortcuts));
            }
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
        let mut dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        dropped.extend(elevated_drop::take());
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
        } else if self.repack.trim().is_empty() && self.running.is_none() {
            (Some(""), true)
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

/// This PC's RAM in whole GB (at least 2).
fn physical_ram_gb() -> u32 {
    let (_, total) = arc_unpack::guard::memory();
    (((total as f64) / (1u64 << 30) as f64).round() as u32).max(2)
}

/// The limit offered when "Limit RAM" is ticked: half of this PC's RAM.
fn default_ram_limit_gb() -> u32 {
    (physical_ram_gb() / 2).max(2)
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
        ui.add_sized([74.0, 32.0], egui::Label::new(RichText::new(label).color(MUTED)));
        let w = ui.available_width() - 104.0;
        let edit = ui.add_enabled(enabled, egui::TextEdit::singleline(value).hint_text(hint).desired_width(w).margin(egui::vec2(10.0, 7.0)));
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

        let logo = self
            .logo
            .get_or_insert_with(|| ctx.load_texture("logo", egui::ColorImage::from_rgba_unmultiplied([128, 128], LOGO), egui::TextureOptions::LINEAR))
            .clone();
        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(22, 18)).show(ui, |ui| {
            // Panels come first: the Install bar keeps the foot of the window.
            if self.offers_install() && !dragging {
                egui::Panel::bottom("install-bar")
                    .frame(egui::Frame::new().inner_margin(egui::Margin { top: 12, ..Default::default() }))
                    .show(ui, |ui| self.install_bar(ui, &ctx));
            }
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(&logo).fit_to_exact_size(egui::vec2(44.0, 44.0)));
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(sb("arc-unpack", 22.0).color(Color32::WHITE));
                    ui.label(RichText::new("Installs repacks faster, using the installer's own steps").color(MUTED));
                });
            });
            ui.add_space(12.0);

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
                    ui.add_space(8.0);
                    let _ = drop_zone(ui, true);
                    return;
                }
                self.before_install(ui, busy);
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
    fn before_install(&mut self, ui: &mut egui::Ui, busy: bool) {
        if self.repack.trim().is_empty() {
            ui.add_space(8.0);
            if drop_zone(ui, false).clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder().and_then(repack_dir_of)
            {
                self.set_repack(&dir);
            }
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

        // While installing, and once that succeeded, the choices can't change any more: one line
        // says what was chosen, and the progress (with its Cancel button) stays in view.
        let installed = matches!(&self.outcome, Some(Outcome::Done { report, .. }) if report.summary.problems.is_empty() && !report.summary.cancelled);
        if busy || installed {
            let mut parts: Vec<String> = a.choices.iter().filter(|c| self.selected.contains(&c.name)).map(|c| c.label.clone()).collect();
            if self.limit_ram {
                parts.push(format!("RAM limited to {} GB", self.ram_limit_gb));
            }
            if !parts.is_empty() {
                ui.label(RichText::new(parts.join(" · ")).color(MUTED));
            }
            return;
        }

        // What to install. Parts that weren't downloaded are greyed out, as in the repack's own
        // installer; a pick-one group (such as the voice language) gets radio buttons.
        let adding = adding_parts(&a, &self.selected);
        if !a.choices.is_empty() {
            let mut toggled: Option<(String, bool, bool)> = None;
            let installed_parts = match &self.check {
                Some(Check { installed: Some(Ok(g)), .. }) if adding => g.parts.clone(),
                _ => BTreeSet::new(),
            };
            card(ui, |ui| {
                ui.label(sb("What to install", 15.0));
                for c in &a.choices {
                    ui.horizontal(|ui| {
                        ui.add_space(22.0 * c.name.matches('\\').count() as f32);
                        let on = self.selected.contains(&c.name);
                        let missing = a.not_downloaded.get(&c.name);
                        let explain = |resp: egui::Response| match missing {
                            Some(files) => {
                                let verb = if files.len() == 1 { "isn't" } else { "aren't" };
                                resp.on_disabled_hover_text(format!("Not downloaded: {} {verb} in the repack folder", files.join(", ")))
                            }
                            None if a.base.contains(&c.name) => resp.on_hover_text("Untick to add only the parts below to a game that's already installed"),
                            None => resp,
                        };
                        if c.exclusive {
                            if explain(ui.add_enabled(missing.is_none(), egui::RadioButton::new(on, &c.label))).clicked() && !on {
                                toggled = Some((c.name.clone(), true, true));
                            }
                        } else {
                            let mut ticked = on;
                            if explain(ui.add_enabled(missing.is_none(), egui::Checkbox::new(&mut ticked, &c.label))).changed() {
                                toggled = Some((c.name.clone(), ticked, false));
                            }
                        }
                        if adding && a.base.contains(&c.name) {
                            ui.label(RichText::new("not reinstalled: adding parts to the installed game").weak());
                        } else if installed_parts.contains(&c.name) {
                            ui.label(RichText::new("already installed").weak());
                        }
                    });
                }
            });
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

        // Options, as the repack's own installer offers them.
        let shortcuts = match &a.inno {
            Some(inno) if !adding => ss::desktop_shortcuts(inno, &self.selected, Path::new(self.target.trim())).len(),
            _ => 0,
        };
        // Runtimes are only offered when this PC lacks something the game needs (most PCs have them all).
        let runtimes = if adding { Vec::new() } else { arc_unpack::runtimes::missing(&a.runtimes) };
        let has_list = a.scan.md5_file.is_some();
        card(ui, |ui| {
            ui.label(sb("Options", 15.0));
            if shortcuts > 0 {
                ui.checkbox(&mut self.create_shortcut, if shortcuts == 1 { "Create a desktop shortcut" } else { "Create desktop shortcuts" });
            }
            if !runtimes.is_empty() {
                let names = arc_unpack::runtimes::labels(&runtimes);
                ui.checkbox(&mut self.install_runtimes, format!("Install missing runtimes: {names}")).on_hover_text(
                    "This PC doesn't have them, and the game may not start without them. They're downloaded from Microsoft \
                     (DirectX's installer comes with the repack), checked for Microsoft's signature, and installed silently \
                     after the game. FitGirl's installer offers the same.",
                );
            }
            ui.horizontal(|ui| {
                let hint = "Tells the decoders to stay within this much RAM and runs one memory-hungry decoder at a time. Slower. A game packed with very large settings can still need more.";
                ui.checkbox(&mut self.limit_ram, "Limit RAM to").on_hover_text(hint);
                ui.add_enabled(self.limit_ram, egui::DragValue::new(&mut self.ram_limit_gb).range(2..=physical_ram_gb()).speed(0.1).suffix(" GB"))
                    .on_hover_text(hint);
            });
            ui.add_enabled(has_list, egui::Checkbox::new(&mut self.skip_check, "Skip archive check")).on_hover_text(if has_list {
                "Don't check the downloaded .bin files before installing. Faster; installed files are still checked as they're written."
            } else {
                "This repack has no MD5 list for its .bin files."
            });
        });
    }

    /// Whether the window offers to install: a repack was read, nothing is running, and it
    /// hasn't just been installed (picking another folder offers it again).
    fn offers_install(&self) -> bool {
        let installed = matches!(&self.outcome, Some(Outcome::Done { report, .. }) if report.summary.problems.is_empty() && !report.summary.cancelled);
        matches!(&self.analysis, Some(Ok(_))) && self.analyzing.is_none() && self.running.is_none() && !installed
    }

    /// The foot of the window before installing: whether everything needed is here, and the
    /// Install button, in view however long the list of parts is.
    fn install_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(Ok(a)) = self.analysis.clone() else { return };
        // Pre-flight: are the needed files here, and does it fit? While a changed selection is
        // re-checked, the previous result stays in place (dimmed if it takes a moment), so
        // nothing moves.
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
                    status(ui, tint(BAD, ui), format!("{e}. Choose the folder the game is installed in, or tick the base game."));
                } else if adding && c.archives == 0 {
                    status(ui, tint(WARN, ui), "Tick the parts to add to the installed game.");
                } else if !c.missing.is_empty() {
                    let (s, it) = if c.missing.len() == 1 { ("", "it") } else { ("s", "them") };
                    status(
                        ui,
                        tint(BAD, ui),
                        format!("Missing file{s}: {}. Download {it} into the repack folder, or untick the part that needs {it}.", c.missing.join(", ")),
                    );
                } else if let Some(e) = &c.error {
                    status(ui, tint(BAD, ui), format!("The installer's steps can't be run: {e}"));
                } else if !fits {
                    status(ui, tint(BAD, ui), format!("Needs {}, but only {} is free on {drive}", human(c.need), human(free.unwrap_or(0))));
                } else {
                    let mut line = if adding {
                        format!("Game found · adds {} · needs {}", part_labels(&a, &self.selected), human(c.need))
                    } else {
                        format!("All needed files are here · needs {}", human(c.need))
                    };
                    if let Some(f) = free {
                        line.push_str(&format!(" · {} free on {drive}", human(f)));
                    }
                    status(ui, tint(OK, ui), line);
                    // Pressing Install mid-check is safe: the install checks for itself.
                    ready = true;
                }
            }
        }
        if let Some(note) = &a.note {
            status(ui, WARN, note.clone());
        }

        // The one primary action.
        ui.add_space(8.0);
        let can = ready && !self.target.trim().is_empty();
        let adding = matches!(&self.check, Some(c) if c.parts.only_optional);
        let label = sb(if adding { "Add to installed game" } else { "Install" }, 17.0);
        let button = egui::Button::new(if can { label.color(Color32::from_rgb(5, 24, 12)) } else { label.color(MUTED) })
            .fill(if can { ACCENT } else { Color32::from_rgb(32, 32, 36) })
            .stroke(egui::Stroke::NONE)
            .corner_radius(10);
        let clicked = ui
            .with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
                ui.add_enabled(can, button.min_size(egui::vec2(0.0, 46.0))).clicked()
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
            _ if cancelled => ("Stopping…".to_string(), String::new()),
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
            ui.add(egui::Spinner::new().size(18.0).color(ACCENT));
            ui.label(sb(&title, 18.0).color(Color32::WHITE));
            // Stop next to the title, where it's always in view.
            if let Some(r) = &self.running
                && !cancelled
            {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Stop installing").clicked() {
                        r.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
        });
        if !sub.is_empty() {
            ui.label(RichText::new(&sub).color(MUTED));
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
            bar(ui, frac as f32, 10.0);
            ui.label(&text);
        }
        ui.horizontal(|ui| {
            if let Some(r) = &self.running {
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
                    ui.label(RichText::new(format!("{} · {}/s", j.label, human(rate as u64))).color(MUTED));
                    bar(ui, d as f32 / t.max(1) as f32, 4.0);
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
                    ("Install stopped".to_string(), WARN)
                } else if ok {
                    (self.adding.as_ref().map(|p| format!("Added {p}")).unwrap_or_else(|| "Installed".to_string()), OK)
                } else {
                    (format!("Finished with {} problem{}", s.problems.len(), if s.problems.len() == 1 { "" } else { "s" }), BAD)
                };
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 28.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.0, color);
                    ui.label(sb(title, 22.0).color(Color32::WHITE));
                });
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
                    status(ui, OK, format!("All {} files match the repack's MD5 list", count(report.files_md5 as u64)));
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
                            self.shortcut_result = Some(make_shortcuts(&report.shortcuts));
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
                ui.label(sb("Install failed", 22.0).color(BAD));
                ui.colored_label(BAD, e);
            }
        }
    }
}
