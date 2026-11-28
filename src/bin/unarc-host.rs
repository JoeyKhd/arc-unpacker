//! unarc-host <unarc.dll> <x|t> <archive> [freearc options...]
//!
//! Runs a whole archive through FreeArc's own unarc.dll. Used for archives whose blocks need
//! FreeArc built-in methods (delta, dispack, 4x4, tor, ...) that arc-unpack can't run as
//! separate stages. unarc.dll is 32-bit, so this is built for i686.
//!
//! API (FreeArc 0.67 Unarc/unarcdll.h):
//!   typedef int __stdcall cbtype(char *what, int int1, int int2, char *str);
//!   extern "C" int __cdecl FreeArcExtract(cbtype *callback, ...);   // NULL-terminated char* args
//! Progress goes to stdout as lines: "total <MB>", "read <MB>", "write <MB>", "file <name>".

use std::ffi::{CStr, CString, c_char};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

type Callback = unsafe extern "system" fn(what: *const c_char, n1: i32, n2: i32, s: *const c_char) -> i32;
type FreeArcExtract = unsafe extern "C" fn(cb: Option<Callback>, ...) -> i32;

static OVERWRITE: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn callback(what: *const c_char, n1: i32, _n2: i32, s: *const c_char) -> i32 {
    let what = if what.is_null() { "" } else { unsafe { CStr::from_ptr(what) }.to_str().unwrap_or("") };
    let text = if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() };
    let mut out = std::io::stdout().lock();
    match what {
        "total" | "read" | "write" => {
            let _ = writeln!(out, "{what} {n1}");
            let _ = out.flush();
            0
        }
        "filename" => {
            let _ = writeln!(out, "file {text}");
            0
        }
        "overwrite?" => (if OVERWRITE.load(Ordering::Relaxed) { b'y' } else { b'n' }) as i32,
        "password?" => b'n' as i32,
        "error" => {
            eprintln!("unarc error {n1}: {text}");
            0
        }
        _ => 0,
    }
}

/// `unarc-host --rar <Unrar.dll> <archive> <out dir> [password]`: extracts a RAR archive with
/// the repack's own Unrar.dll (RARLAB's unrar.dll API, as ISDone's ISRarExtract does).
/// Prints "file <name>" per file; exits 1 on an error, with the reason on stderr.
fn rar(args: &[String]) -> ! {
    use std::os::windows::ffi::OsStrExt;
    type OpenEx = unsafe extern "system" fn(data: *mut u8) -> isize;
    type Close = unsafe extern "system" fn(h: isize) -> i32;
    type ReadHeaderEx = unsafe extern "system" fn(h: isize, header: *mut u8) -> i32;
    type ProcessFileW = unsafe extern "system" fn(h: isize, op: i32, dest_path: *const u16, dest_name: *const u16) -> i32;
    type SetPassword = unsafe extern "system" fn(h: isize, password: *const c_char);
    const RAR_OM_EXTRACT: u32 = 1;
    const RAR_EXTRACT: i32 = 2;
    const ERAR_END_ARCHIVE: i32 = 10;
    let fail = |msg: String| -> ! {
        eprintln!("unarc-host: {msg}");
        std::process::exit(1)
    };
    if args.len() < 3 {
        fail("usage: unarc-host --rar <Unrar.dll> <archive> <out dir> [password]".into());
    }
    let wide = |s: &str| -> Vec<u16> { std::ffi::OsStr::new(s).encode_wide().chain([0]).collect() };
    let lib = unsafe { libloading::Library::new(&args[0]) }.unwrap_or_else(|e| fail(format!("cannot load {}: {e}", args[0])));
    let (open, close, read, process, set_password) = unsafe {
        let get = |name: &[u8]| lib.get::<*const ()>(name).map(|s| *s).unwrap_or_else(|e| fail(format!("{}: {e}", String::from_utf8_lossy(name))));
        (
            std::mem::transmute::<*const (), OpenEx>(get(b"RAROpenArchiveEx\0")),
            std::mem::transmute::<*const (), Close>(get(b"RARCloseArchive\0")),
            std::mem::transmute::<*const (), ReadHeaderEx>(get(b"RARReadHeaderEx\0")),
            std::mem::transmute::<*const (), ProcessFileW>(get(b"RARProcessFileW\0")),
            std::mem::transmute::<*const (), SetPassword>(get(b"RARSetPassword\0")),
        )
    };
    let archive = wide(&args[1]);
    let dest = wide(&args[2]);
    // RAROpenArchiveDataEx (32-bit): ArcName, ArcNameW, OpenMode, OpenResult, ... The rest stays
    // zero; the buffer is larger than any version of the structure.
    let mut open_data = vec![0u8; 512];
    open_data[4..8].copy_from_slice(&(archive.as_ptr() as u32).to_le_bytes());
    open_data[8..12].copy_from_slice(&RAR_OM_EXTRACT.to_le_bytes());
    let h = unsafe { open(open_data.as_mut_ptr()) };
    let open_result = u32::from_le_bytes(open_data[12..16].try_into().unwrap());
    if h == 0 || open_result != 0 {
        fail(format!("can't open {} as a RAR archive (error {open_result})", args[1]));
    }
    if let Some(pw) = args.get(3).filter(|p| !p.is_empty()) {
        let pw = CString::new(pw.as_str()).unwrap_or_default();
        unsafe { set_password(h, pw.as_ptr()) };
    }
    // RARHeaderDataEx: ArcName[1024], ArcNameW[1024], FileName[1024], FileNameW[1024], ...
    let mut header = vec![0u8; 16384];
    let mut out = std::io::stdout().lock();
    let mut files = 0u64;
    loop {
        match unsafe { read(h, header.as_mut_ptr()) } {
            0 => {}
            ERAR_END_ARCHIVE => break,
            e => {
                unsafe { close(h) };
                fail(format!("reading {}: unrar error {e}", args[1]));
            }
        }
        let name: Vec<u16> = header[4096..4096 + 2048].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        let name = String::from_utf16_lossy(&name);
        let rc = unsafe { process(h, RAR_EXTRACT, dest.as_ptr(), std::ptr::null()) };
        if rc != 0 {
            unsafe { close(h) };
            fail(format!("extracting {name} from {}: unrar error {rc}", args[1]));
        }
        files += 1;
        let _ = writeln!(out, "file {name}");
    }
    unsafe { close(h) };
    let _ = writeln!(out, "done {files}");
    let _ = out.flush();
    std::process::exit(0)
}

/// Loads the CLS proxy DLLs in `dir` that start a helper program (`<stem>_x86.exe` or
/// `_x64.exe`, with or without the "cls-" prefix) and hooks their channel claim
/// (`guard::hook_channel_claims`). They stay loaded.
fn preload_proxies(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if !(lower.starts_with("cls-") && lower.ends_with(".dll")) {
            continue;
        }
        let stem = &name[..name.len() - 4];
        let helper = ["x86", "x64"].iter().any(|a| dir.join(format!("{stem}_{a}.exe")).is_file() || dir.join(format!("{}_{a}.exe", &stem[4..])).is_file());
        if !helper || !arc_unpack::guard::imports_channel_claims(&e.path()) {
            continue;
        }
        if let Ok(lib) = unsafe { libloading::os::windows::Library::new(e.path()) } {
            let base = lib.into_raw() as *mut std::ffi::c_void;
            if !unsafe { arc_unpack::guard::hook_channel_claims(base) } {
                eprintln!("unarc-host: {name}: its channel claim couldn't be hooked");
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--rar") {
        rar(&args[1..]);
    }
    if args.len() < 3 || args.len() > 14 {
        eprintln!("usage: unarc-host <unarc.dll> <x|t> <archive> [options...]   (at most 11 options)");
        std::process::exit(64);
    }
    // unarc's external codecs (and their helpers) die with us.
    arc_unpack::guard::kill_descendants_on_exit();
    // unarc loads CLS proxies in-process; CLS-srep reads its helper's prefix from here.
    arc_unpack::guard::provide_cls_proxy_init();
    // The CLS proxies unarc loads may start helpers that inherit handles: not our pipes
    // (stderr stays inheritable: unarc's external codecs report their errors there).
    arc_unpack::guard::keep_std_handles_private(false);
    OVERWRITE.store(args.iter().any(|a| a == "-o+"), Ordering::Relaxed);

    let lib = match unsafe { libloading::Library::new(&args[0]) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("unarc-host: cannot load {}: {e}", args[0]);
            std::process::exit(65);
        }
    };
    // The CLS proxies with a helper program claim a numbered channel when they start, which
    // unarc.dll does at any time: loaded now (unarc gets the same modules) and hooked, they
    // hold arc-unpack's codec start-up lock just while they claim, like the codec host does.
    preload_proxies(std::path::Path::new(&args[0]).parent().unwrap_or(std::path::Path::new(".")));
    let extract: libloading::Symbol<FreeArcExtract> = match unsafe { lib.get(b"FreeArcExtract\0") } {
        Ok(f) => f,
        Err(e) => {
            eprintln!("unarc-host: no FreeArcExtract export: {e}");
            std::process::exit(65);
        }
    };

    // FreeArc command line: <cmd> [options] -- <archive>
    let mut cmd: Vec<CString> = Vec::new();
    cmd.push(CString::new(args[1].as_str()).unwrap());
    for o in &args[3..] {
        cmd.push(CString::new(o.as_str()).unwrap());
    }
    cmd.push(CString::new("--").unwrap());
    cmd.push(CString::new(args[2].as_str()).unwrap());
    let mut p: Vec<*const c_char> = cmd.iter().map(|c| c.as_ptr()).collect();
    p.resize(16, std::ptr::null());

    let rc = unsafe {
        extract(
            Some(callback),
            p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9], p[10], p[11], p[12], p[13], p[14], p[15],
            std::ptr::null::<c_char>(),
        )
    };
    let _ = std::io::stdout().flush();
    if rc != 0 {
        eprintln!("unarc-host: FreeArcExtract returned {rc}");
        std::process::exit(1);
    }
}
