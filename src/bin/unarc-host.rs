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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 || args.len() > 14 {
        eprintln!("usage: unarc-host <unarc.dll> <x|t> <archive> [options...]   (at most 11 options)");
        std::process::exit(64);
    }
    // unarc's external codecs (and their helpers) die with us.
    arc_unpack::guard::kill_descendants_on_exit();
    // unarc loads CLS proxies in-process; CLS-srep reads its helper's prefix from here.
    arc_unpack::guard::provide_cls_proxy_init();
    OVERWRITE.store(args.iter().any(|a| a == "-o+"), Ordering::Relaxed);

    let lib = match unsafe { libloading::Library::new(&args[0]) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("unarc-host: cannot load {}: {e}", args[0]);
            std::process::exit(65);
        }
    };
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
