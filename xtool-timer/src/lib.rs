//! A do-nothing plugin for xtool (the precompressor many repacks use).
//!
//! xtool coordinates its threads with `Sleep(1)` polling: a worker notices new work, and the
//! main thread notices finished work, only at the next wake-up. At Windows' default timer
//! resolution `Sleep(1)` lasts about 15.6 ms, and xtool never asks for a finer one, so every
//! chunk it decodes spends several of those ticks with all its workers idle.
//!
//! xtool loads every DLL next to it that exports `PrecompInit` and `PrecompCodec`. This one
//! exports both, registers no codecs (xtool drops it from its table before decoding, because
//! no stream names it), and its only effect is the request made when it is loaded: 1 ms timer
//! resolution for xtool's own process. Timer requests are per process, so nothing else on the
//! machine changes, and it ends when xtool exits.

use core::ffi::c_void;

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
}

const DLL_PROCESS_ATTACH: u32 = 1;

/// # Safety
/// Called by the Windows loader.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(_module: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe { timeBeginPeriod(1) };
    }
    1
}

/// xtool's plugin entry point. Returning false means "not available": xtool only calls it for
/// plugins a stream names, which is never this one.
#[unsafe(no_mangle)]
pub extern "C" fn PrecompInit(_command: *const u16, _count: i32, _funcs: *const c_void) -> bool {
    false
}

/// Name of codec number `index`: none.
#[unsafe(no_mangle)]
pub extern "C" fn PrecompCodec(_index: i32) -> *const u16 {
    core::ptr::null()
}
