//! The uninstaller arc-unpack puts in every game folder as `uninstall.exe` (see
//! `arc_unpack::uninstall`). Windows' list of installed apps runs it; `/S` uninstalls without
//! asking.
//!
//! Run from the game folder, it copies itself to the temporary folder, runs that copy and waits
//! for it, so whoever started it knows when it's done and the copy in the game folder can go too.
//! At the end, a short-lived `cmd` removes that copy, the temporary one and the game folder (if
//! the install made it and it is empty).
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    std::process::exit(win::run());
    #[cfg(not(windows))]
    std::process::exit(1);
}

#[cfg(windows)]
mod win {
    use arc_unpack::extract::human;
    use arc_unpack::uninstall::{self, Record, Removal};
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
    use windows_sys::Win32::UI::Controls::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDCANCEL, SendMessageW};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const UNINSTALL: i32 = 100;
    const OPEN_FOLDER: i32 = 101;
    const TITLE: &str = "Uninstall";
    /// Exit codes: done, something couldn't be removed, not uninstalled (cancelled or no list).
    const DONE: i32 = 0;
    const PROBLEMS: i32 = 1;
    const NOT_DONE: i32 = 2;

    struct Args {
        quiet: bool,
        /// The game folder (when running from the temporary copy).
        from: Option<PathBuf>,
    }

    fn args() -> Args {
        let mut a = Args { quiet: false, from: None };
        let mut it = std::env::args_os().skip(1);
        while let Some(x) = it.next() {
            match x.to_string_lossy().to_ascii_lowercase().as_str() {
                // Inno's switches, so tools that uninstall repacks silently work as they do there.
                "/s" | "/silent" | "/verysilent" | "--quiet" => a.quiet = true,
                "--from" => a.from = it.next().map(PathBuf::from),
                _ => {}
            }
        }
        a
    }

    pub fn run() -> i32 {
        // SAFETY: called once, before any window exists.
        unsafe { windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let args = args();
        let me = std::env::current_exe().unwrap_or_default();
        match &args.from {
            Some(dir) => work(dir, args.quiet, Some(&me)),
            None => from_temp_copy(&me, args.quiet),
        }
    }

    /// Runs a temporary copy of this program on the game folder and waits for it.
    fn from_temp_copy(me: &Path, quiet: bool) -> i32 {
        let dir = me.parent().map(Path::to_path_buf).unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!("arc-unpack-uninstall-{}.exe", std::process::id()));
        if std::fs::copy(me, &tmp).is_err() {
            return work(&dir, quiet, None);
        }
        let mut cmd = std::process::Command::new(&tmp);
        cmd.arg("--from").arg(&dir).current_dir(std::env::temp_dir());
        if quiet {
            cmd.arg("/S");
        }
        match cmd.spawn() {
            Ok(mut child) => child.wait().ok().and_then(|s| s.code()).unwrap_or(PROBLEMS),
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
                work(&dir, quiet, None)
            }
        }
    }

    fn work(dir: &Path, quiet: bool, tmp: Option<&Path>) -> i32 {
        let rec = match uninstall::read(dir) {
            Ok(r) => r,
            Err(e) => {
                if !quiet {
                    dialog(&Dialog {
                        instruction: "Nothing was uninstalled",
                        content: &format!("{} has no list of what to remove ({e:#}).", dir.display()),
                        ..Dialog::default()
                    });
                }
                return NOT_DONE;
            }
        };
        // An entry or shortcuts for all users need administrator rights.
        if rec.needs_admin() && !arc_unpack::guard::is_elevated() {
            return match run_elevated(dir, quiet) {
                Some(code) => code,
                None => {
                    if !quiet {
                        dialog(&Dialog {
                            instruction: &format!("{} was not uninstalled", rec.name),
                            content: "Uninstalling it needs administrator rights, as installing it did.",
                            ..Dialog::default()
                        });
                    }
                    NOT_DONE
                }
            };
        }
        if !quiet && !confirm(&rec, dir) {
            return NOT_DONE;
        }
        let mut removal = if quiet { uninstall::remove(dir, &rec, &mut |_, _| {}) } else { remove_with_progress(dir, &rec) };
        if let Some(e) = &rec.apps_entry
            && let Err(err) = uninstall::apps::unregister(e)
        {
            removal.failed.push(format!("its entry in Windows' list of installed apps: {err:#}"));
        }
        if !quiet {
            finished(&rec, dir, &removal);
        }
        clean_up_later(dir, &rec, &removal, tmp);
        if removal.failed.is_empty() { DONE } else { PROBLEMS }
    }

    fn confirm(rec: &Record, dir: &Path) -> bool {
        let files = rec.files.len();
        let shortcuts = match rec.shortcuts.len() {
            0 => String::new(),
            1 => " and its shortcut".into(),
            _ => " and its shortcuts".into(),
        };
        let content = format!(
            "This removes its {} file{} ({}) from {}{shortcuts}. Saves and settings the game made itself are kept.",
            count(files),
            if files == 1 { "" } else { "s" },
            human(rec.bytes),
            dir.display()
        );
        let pressed = dialog(&Dialog {
            instruction: &format!("Uninstall {}?", rec.name),
            content: &content,
            buttons: &[(UNINSTALL, "Uninstall")],
            common: TDCBF_CANCEL_BUTTON,
            default: UNINSTALL,
            ..Dialog::default()
        });
        pressed == UNINSTALL
    }

    /// Shared between the progress dialog and the thread that removes the files.
    struct Progress {
        done: AtomicUsize,
        total: AtomicUsize,
        finished: AtomicBool,
    }

    fn remove_with_progress(dir: &Path, rec: &Record) -> Removal {
        let progress = Arc::new(Progress { done: AtomicUsize::new(0), total: AtomicUsize::new(rec.files.len().max(1)), finished: AtomicBool::new(false) });
        let worker = {
            let (dir, rec, p) = (dir.to_path_buf(), rec.clone(), progress.clone());
            std::thread::spawn(move || {
                let r = uninstall::remove(&dir, &rec, &mut |done, total| {
                    p.done.store(done, Ordering::Relaxed);
                    p.total.store(total.max(1), Ordering::Relaxed);
                });
                p.finished.store(true, Ordering::Release);
                r
            })
        };
        dialog(&Dialog {
            instruction: &format!("Uninstalling {}…", rec.name),
            content: "Removing its files.",
            common: TDCBF_CANCEL_BUTTON,
            progress: Some((on_progress, Arc::as_ptr(&progress) as isize)),
            ..Dialog::default()
        });
        worker.join().unwrap_or_default()
    }

    /// The progress dialog's callback: moves the bar, closes the dialog when the removal is done,
    /// and keeps it open until then (its Cancel button is disabled: a half-removed game helps no one).
    unsafe extern "system" fn on_progress(hwnd: HWND, msg: u32, _: WPARAM, _: LPARAM, data: isize) -> windows_sys::core::HRESULT {
        // SAFETY: `data` is the `Progress` that `remove_with_progress` keeps alive while the dialog is open.
        let p = unsafe { &*(data as *const Progress) };
        let done = p.finished.load(Ordering::Acquire);
        match msg as i32 {
            TDN_CREATED => unsafe {
                SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, IDCANCEL as WPARAM, 0);
            },
            TDN_TIMER => unsafe {
                let pos = p.done.load(Ordering::Relaxed) * 100 / p.total.load(Ordering::Relaxed).max(1);
                SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_POS as u32, pos.min(100), 0);
                if done {
                    SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, IDCANCEL as WPARAM, 1);
                    SendMessageW(hwnd, TDM_CLICK_BUTTON as u32, IDCANCEL as WPARAM, 0);
                }
            },
            // S_FALSE keeps the dialog open.
            TDN_BUTTON_CLICKED if !done => return 1,
            _ => {}
        }
        0
    }

    fn finished(rec: &Record, dir: &Path, r: &Removal) {
        let (instruction, content) = if !r.failed.is_empty() {
            let shown: Vec<&str> = r.failed.iter().take(5).map(String::as_str).collect();
            let more = if r.failed.len() > 5 { format!("\n… and {} more", r.failed.len() - 5) } else { String::new() };
            (
                format!("{} was uninstalled, except for what couldn't be removed", rec.name),
                format!("Close the game if it's running, then delete these by hand:\n{}{more}", shown.join("\n")),
            )
        } else if !r.kept.is_empty() {
            let n = r.kept.len();
            (
                format!("{} was uninstalled", rec.name),
                format!(
                    "{} file{} the game made itself, such as saves and settings, {} still in {}.",
                    count(n),
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "is" } else { "are" },
                    dir.display()
                ),
            )
        } else {
            (format!("{} was uninstalled", rec.name), String::new())
        };
        let left = !r.failed.is_empty() || !r.kept.is_empty();
        let buttons: &[(i32, &str)] = if left { &[(OPEN_FOLDER, "Open folder")] } else { &[] };
        let pressed = dialog(&Dialog { instruction: &instruction, content: &content, buttons, common: TDCBF_CLOSE_BUTTON, ..Dialog::default() });
        if pressed == OPEN_FOLDER {
            let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
        }
    }

    /// Once this program has exited: removes `uninstall.exe` from the game folder, the folder
    /// itself if the install made it and nothing is left in it, and the temporary copy. Tried
    /// every second for up to half a minute: Explorer can hold the folder for a moment after its
    /// shortcuts are deleted.
    fn clean_up_later(dir: &Path, rec: &Record, removal: &Removal, tmp: Option<&Path>) {
        let exe = dir.join(uninstall::EXE);
        let folder_goes = rec.made_folder && removal.kept.is_empty() && removal.failed.is_empty();
        let mut step = format!("ping -n 2 127.0.0.1 >nul & del /f /q \"{}\" 2>nul", exe.display());
        let mut done = format!("if not exist \"{}\"", exe.display());
        if folder_goes {
            step.push_str(&format!(" & rd \"{}\" 2>nul", dir.display()));
            done.push_str(&format!(" if not exist \"{}\"", dir.display()));
        }
        if let Some(t) = tmp {
            step.push_str(&format!(" & del /f /q \"{}\" 2>nul", t.display()));
            done.push_str(&format!(" if not exist \"{}\"", t.display()));
        }
        let line = format!("/C \"for /l %i in (1,1,30) do @({step} & {done} exit)\"");
        let _ = std::process::Command::new("cmd.exe").raw_arg(line).current_dir(std::env::temp_dir()).creation_flags(CREATE_NO_WINDOW).spawn();
    }

    /// Runs this program again with administrator rights (Windows asks) and waits for it. None
    /// if it couldn't start (e.g. the request was declined).
    fn run_elevated(dir: &Path, quiet: bool) -> Option<i32> {
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
        use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
        let me = wide(&std::env::current_exe().ok()?.to_string_lossy());
        let verb = wide("runas");
        let dir = dir.to_string_lossy();
        let params = wide(&format!("--from \"{}\"{}", dir.trim_end_matches('\\'), if quiet { " /S" } else { "" }));
        // SAFETY: the strings outlive the call; the struct is plain data.
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS;
            info.lpVerb = verb.as_ptr();
            info.lpFile = me.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = 1;
            if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
                return None;
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = PROBLEMS as u32;
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            Some(code as i32)
        }
    }

    struct Dialog<'a> {
        instruction: &'a str,
        content: &'a str,
        buttons: &'a [(i32, &'a str)],
        common: TASKDIALOG_COMMON_BUTTON_FLAGS,
        default: i32,
        /// A progress bar, moved by this callback with this data.
        progress: Option<(unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, isize) -> windows_sys::core::HRESULT, isize)>,
    }

    impl Default for Dialog<'_> {
        fn default() -> Self {
            Dialog { instruction: "", content: "", buttons: &[], common: TDCBF_CLOSE_BUTTON, default: 0, progress: None }
        }
    }

    /// Shows a task dialog; returns the button pressed (IDCANCEL when closed).
    fn dialog(d: &Dialog) -> i32 {
        let (title, instruction, content) = (wide(TITLE), wide(d.instruction), wide(d.content));
        let texts: Vec<Vec<u16>> = d.buttons.iter().map(|(_, t)| wide(t)).collect();
        let buttons: Vec<TASKDIALOG_BUTTON> = d.buttons.iter().zip(&texts).map(|((id, _), t)| TASKDIALOG_BUTTON { nButtonID: *id, pszButtonText: t.as_ptr() }).collect();
        let mut cfg = TASKDIALOGCONFIG { cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32, ..Default::default() };
        cfg.dwCommonButtons = d.common;
        cfg.pszWindowTitle = title.as_ptr();
        cfg.pszMainInstruction = instruction.as_ptr();
        if !d.content.is_empty() {
            cfg.pszContent = content.as_ptr();
        }
        cfg.cButtons = buttons.len() as u32;
        cfg.pButtons = buttons.as_ptr();
        cfg.nDefaultButton = d.default;
        cfg.dwFlags = match d.progress {
            Some((callback, data)) => {
                cfg.pfCallback = Some(callback);
                cfg.lpCallbackData = data;
                TDF_SHOW_PROGRESS_BAR | TDF_CALLBACK_TIMER
            }
            None => TDF_ALLOW_DIALOG_CANCELLATION,
        };
        let mut pressed = IDCANCEL;
        // SAFETY: every string and the button array outlive the (modal) call.
        let hr = unsafe { TaskDialogIndirect(&cfg, &mut pressed, std::ptr::null_mut(), std::ptr::null_mut()) };
        if hr < 0 { IDCANCEL } else { pressed }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// "37,813".
    fn count(n: usize) -> String {
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        out
    }
}
