//! cls-host <codec.dll> <method>   (e.g. cls-host cls-lolz.dll lolz:d64m:mtt1)
//!
//! Loads a FreeArc CLS codec DLL and decompresses stdin to stdout through it. Built twice
//! (x86 and x64), because a process can only load DLLs of its own architecture and most CLS
//! codecs in the wild are 32-bit.
//!
//! Interface, from FreeArc 0.67 Compression/_CLS/cls.h and Compression/CLS/C_CLS.cpp:
//!   int __cdecl ClsMain(int op, CLS_CALLBACK cb, void* instance)
//!   int __cdecl CLS_CALLBACK(void* instance, int op, void* ptr, int n)
//! The host calls ClsMain(CLS_INIT, NULL, NULL) once, ClsMain(CLS_DECOMPRESS, cb, inst)
//! per stream, then ClsMain(CLS_DONE, NULL, NULL). This mirrors what FreeArc itself does.

use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;

const CLS_INIT: i32 = 1;
const CLS_DONE: i32 = 2;
const CLS_DECOMPRESS: i32 = 4;

const CLS_MALLOC: i32 = 1;
const CLS_FREE: i32 = 2;
const CLS_GET_PARAMSTR: i32 = 3;
const CLS_FULL_READ: i32 = 4096;
const CLS_PARTIAL_READ: i32 = 5120;
const CLS_FULL_WRITE: i32 = 6144;
const CLS_PARTIAL_WRITE: i32 = 7168;

const CLS_OK: i32 = 0;
const CLS_ERROR_NOT_IMPLEMENTED: i32 = -2;
const CLS_ERROR_NOT_ENOUGH_MEMORY: i32 = -3;
/// FreeArc's host-side codes, which is what DLLs see coming back from read/write.
const FREEARC_ERRCODE_READ: i32 = -6;
const FREEARC_ERRCODE_WRITE: i32 = -11;
const FREEARC_ERRCODE_NO_MORE_DATA_REQUIRED: i32 = -9;

type Callback = unsafe extern "C" fn(instance: *mut c_void, op: i32, ptr: *mut c_void, n: i32) -> i32;
type ClsMain = unsafe extern "C" fn(op: i32, cb: Option<Callback>, instance: *mut c_void) -> i32;

unsafe extern "C" {
    fn malloc(n: usize) -> *mut c_void;
    fn free(p: *mut c_void);
}

struct Host {
    params: Vec<u8>, // NUL-terminated
    input: ManuallyDrop<std::fs::File>,
    output: io::BufWriter<&'static std::fs::File>,
    downstream_closed: bool,
    error: Option<String>,
}

/// Set ARC_UNPACK_CLS_TRACE=1 to log every callback the codec makes (to stderr).
static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static IO_CALLS: std::sync::Mutex<Vec<(i32, u64, u64)>> = std::sync::Mutex::new(Vec::new()); // (op, calls, bytes)

unsafe extern "C" fn callback(instance: *mut c_void, op: i32, ptr: *mut c_void, n: i32) -> i32 {
    let ret = unsafe { callback_inner(instance, op, ptr, n) };
    if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
        if matches!(op, CLS_FULL_READ | CLS_PARTIAL_READ | CLS_FULL_WRITE | CLS_PARTIAL_WRITE) {
            let mut io = IO_CALLS.lock().unwrap();
            match io.iter_mut().find(|e| e.0 == op) {
                Some(e) => {
                    e.1 += 1;
                    e.2 += ret.max(0) as u64;
                }
                None => {
                    eprintln!("[cls trace] first op={op} n={n} -> {ret}");
                    io.push((op, 1, ret.max(0) as u64));
                }
            }
            if ret <= 0 {
                eprintln!("[cls trace] op={op} n={n} -> {ret}");
            }
        } else {
            eprintln!("[cls trace] op={op} n={n} -> {ret}");
        }
    }
    ret
}

unsafe fn callback_inner(instance: *mut c_void, op: i32, ptr: *mut c_void, n: i32) -> i32 {
    let host = unsafe { &mut *(instance as *mut Host) };
    match op {
        CLS_GET_PARAMSTR => {
            // FreeArc does a plain strcpy and ignores n; do the same.
            unsafe { std::ptr::copy_nonoverlapping(host.params.as_ptr(), ptr as *mut u8, host.params.len()) };
            CLS_OK
        }
        CLS_FULL_READ | CLS_PARTIAL_READ => {
            if n <= 0 {
                return 0;
            }
            let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, n as usize) };
            // FreeArc fills the buffer completely unless the stream ends, for both read kinds.
            let mut got = 0;
            while got < buf.len() {
                match host.input.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(k) => got += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break, // writer closed = EOF
                    Err(e) => {
                        host.error = Some(format!("read stdin: {e}"));
                        return FREEARC_ERRCODE_READ;
                    }
                }
            }
            got as i32
        }
        CLS_FULL_WRITE | CLS_PARTIAL_WRITE => {
            if n <= 0 {
                return 0;
            }
            let buf = unsafe { std::slice::from_raw_parts(ptr as *const u8, n as usize) };
            match host.output.write_all(buf) {
                Ok(()) => n,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    // The consumer has everything it needs (or failed and will report it).
                    host.downstream_closed = true;
                    FREEARC_ERRCODE_NO_MORE_DATA_REQUIRED
                }
                Err(e) => {
                    host.error = Some(format!("write stdout: {e}"));
                    FREEARC_ERRCODE_WRITE
                }
            }
        }
        CLS_MALLOC => {
            let p = unsafe { malloc(n.max(0) as usize) };
            unsafe { *(ptr as *mut *mut c_void) = p };
            if p.is_null() { CLS_ERROR_NOT_ENOUGH_MEMORY } else { CLS_OK }
        }
        CLS_FREE => {
            unsafe { free(ptr) };
            CLS_OK
        }
        _ => CLS_ERROR_NOT_IMPLEMENTED,
    }
}

fn error_name(code: i32) -> &'static str {
    match code {
        -1 => "general error",
        -2 => "invalid compressor / unsupported method",
        -3 => "only decompression supported",
        -4 => "output block too small",
        -5 => "not enough memory",
        -6 => "read error",
        -7 => "bad compressed data",
        -8 => "not implemented",
        -9 => "no more data required",
        -10 => "operation terminated",
        -11 => "write error",
        -12 => "bad CRC",
        _ => "unknown error",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: cls-host <cls-xxx.dll> <method[:params]>   (stdin -> stdout)");
        std::process::exit(64);
    }
    // Our codec's helper processes (e.g. a proxy's 64-bit .exe) die with us.
    arc_unpack::guard::kill_descendants_on_exit();
    // Some CLS proxies (CLS-srep) read their helper's shared-memory prefix from here.
    arc_unpack::guard::provide_cls_proxy_init();
    let dll_path = &args[1];
    let method = &args[2];
    // ClsMain receives only the params ("srep:m3yf" -> "m3yf"), joined with ':'.
    let params = method.split_once(':').map(|(_, p)| p).unwrap_or("");
    let mut params_z = params.as_bytes().to_vec();
    params_z.push(0);

    let lib = match unsafe { libloading::Library::new(dll_path) } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cls-host: cannot load {dll_path}: {e}");
            std::process::exit(65);
        }
    };
    let cls_main: libloading::Symbol<ClsMain> = match unsafe { lib.get(b"ClsMain\0") } {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cls-host: {dll_path} has no ClsMain export: {e}");
            std::process::exit(65);
        }
    };

    #[cfg(windows)]
    let (input, output) = {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        unsafe {
            (
                ManuallyDrop::new(std::fs::File::from_raw_handle(io::stdin().as_raw_handle())),
                &*Box::leak(Box::new(std::fs::File::from_raw_handle(io::stdout().as_raw_handle()))),
            )
        }
    };
    #[cfg(unix)]
    let (input, output) = {
        use std::os::fd::FromRawFd;
        unsafe { (ManuallyDrop::new(std::fs::File::from_raw_fd(0)), &*Box::leak(Box::new(std::fs::File::from_raw_fd(1)))) }
    };

    let mut host = Host {
        params: params_z,
        input,
        output: io::BufWriter::with_capacity(1 << 20, output),
        downstream_closed: false,
        error: None,
    };

    TRACE.store(std::env::var_os("ARC_UNPACK_CLS_TRACE").is_some(), std::sync::atomic::Ordering::Relaxed);
    let ret = unsafe {
        cls_main(CLS_INIT, None, std::ptr::null_mut());
        let r = cls_main(CLS_DECOMPRESS, Some(callback), &mut host as *mut Host as *mut c_void);
        cls_main(CLS_DONE, None, std::ptr::null_mut());
        r
    };

    let flushed = host.output.flush();
    if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[cls trace] ClsMain(DECOMPRESS) = {ret}; io totals (op, calls, bytes): {:?}", IO_CALLS.lock().unwrap());
    }
    if let Some(e) = &host.error {
        eprintln!("cls-host: {method}: {e}");
        std::process::exit(1);
    }
    if ret < 0 && ret != FREEARC_ERRCODE_NO_MORE_DATA_REQUIRED {
        eprintln!("cls-host: {method}: ClsMain returned {ret} ({})", error_name(ret));
        std::process::exit(1);
    }
    if let Err(e) = flushed
        && !host.downstream_closed
    {
        eprintln!("cls-host: {method}: flush stdout: {e}");
        std::process::exit(1);
    }
}
