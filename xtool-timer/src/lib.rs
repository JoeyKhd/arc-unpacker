//! A small plugin for xtool (the precompressor many repacks use). It registers no codecs; it
//! only changes two things inside xtool's own process when xtool loads it.
//!
//! xtool loads every DLL next to it that exports `PrecompInit` and `PrecompCodec`. This one
//! exports both and registers no codecs (xtool drops it from its table before decoding, because
//! no stream names it). Its effects, made when it is loaded:
//!
//! 1. **1 ms timer resolution.** xtool coordinates its threads with `Sleep(1)` polling: a worker
//!    notices new work, and the main thread notices finished work, only at the next wake-up. At
//!    Windows' default timer resolution `Sleep(1)` lasts about 15.6 ms, and xtool never asks for
//!    a finer one. Timer requests are per process and end when xtool exits.
//!
//! 2. **A steady view of free memory.** xtool keeps the streams it must repeat (deduplication)
//!    in a store made of a memory part and a spill part, and lets the memory part grow by the
//!    free memory it sees each time it adds a stream. Positions in the store are mapped through
//!    the parts' sizes, so when free memory is low for a while (the memory part fills, streams
//!    spill) and then rises (the memory part grows), the spilled streams are read back from the
//!    wrong place: the largest repeated files are restored wrong, without any error. That
//!    happens whenever other programs free memory while xtool runs. Here xtool's calls to
//!    `GlobalMemoryStatusEx` get the figures from when it started. With those, the memory part
//!    grows by the same amount C at every stream it adds. Streams are under 2 GiB, so for C = 0
//!    (everything goes to the spill part) or C >= 2 GiB (the memory part never fills) xtool gets
//!    it right. A C in between (a loaded or small PC: used memory just under the -dm share, or
//!    free memory just over 1 GiB) can still fill, spill and grow; then the snapshot reports at
//!    most 1 GiB free, which makes xtool's C exactly 0 (reverse-engineered: CalcDupSysMem, the
//!    same in xtool 0.7.9, 0.8.8, 0.9.2 and rtool 0.8.9). Spilled or not, the same bytes come back.
//!    `ARC_UNPACK_XTOOL_LIVE_MEMORY` (set to anything) turns this off.

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut u8;
    fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
    fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32;
    fn GetCommandLineW() -> *const u16;
}

const DLL_PROCESS_ATTACH: u32 = 1;
const PAGE_READWRITE: u32 = 0x04;

/// The figures from when xtool loaded this plugin.
static mut SNAPSHOT: MemoryStatusEx = MemoryStatusEx {
    length: 0,
    memory_load: 0,
    total_phys: 0,
    avail_phys: 0,
    total_page_file: 0,
    avail_page_file: 0,
    total_virtual: 0,
    avail_virtual: 0,
    avail_extended_virtual: 0,
};
/// Set once the snapshot is taken (only then are xtool's calls redirected).
static READY: AtomicUsize = AtomicUsize::new(0);

/// # Safety
/// Called by the Windows loader.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(_module: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            timeBeginPeriod(1);
            if !env_set("ARC_UNPACK_XTOOL_LIVE_MEMORY") {
                steady_memory_view();
            }
        }
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

/// `GlobalMemoryStatusEx` as xtool sees it: the figures from when it started.
unsafe extern "system" fn steady_global_memory_status_ex(status: *mut MemoryStatusEx) -> i32 {
    if status.is_null() {
        return 0;
    }
    unsafe {
        if READY.load(Ordering::Acquire) == 0 {
            return GlobalMemoryStatusEx(status);
        }
        let length = (*status).length;
        *status = *core::ptr::addr_of!(SNAPSHOT);
        (*status).length = length;
    }
    1
}

fn env_set(name: &str) -> bool {
    let mut wide = [0u16; 64];
    for (i, c) in name.encode_utf16().take(63).enumerate() {
        wide[i] = c;
    }
    let mut buf = [0u16; 4];
    unsafe { GetEnvironmentVariableW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) > 0 }
}

/// Takes the snapshot and points xtool's import of kernel32's `GlobalMemoryStatusEx` at
/// `steady_global_memory_status_ex`. Leaves everything as it is if the executable doesn't look
/// as expected (only a 64-bit executable that imports the function by name is changed).
unsafe fn steady_memory_view() {
    unsafe {
        let base = GetModuleHandleW(core::ptr::null());
        if base.is_null() {
            return;
        }
        let snapshot = core::ptr::addr_of_mut!(SNAPSHOT);
        (*snapshot).length = core::mem::size_of::<MemoryStatusEx>() as u32;
        if GlobalMemoryStatusEx(snapshot) == 0 {
            return;
        }
        let cmd = GetCommandLineW();
        if !cmd.is_null() {
            let mut n = 0;
            while *cmd.add(n) != 0 {
                n += 1;
            }
            let line = core::slice::from_raw_parts(cmd, n);
            if let Some(pct) = dedup_percent(line) {
                (*snapshot).avail_phys = avoid_small_budget((*snapshot).total_phys, (*snapshot).avail_phys, pct);
            }
        }
        let Some(slot) = import_slot(base, b"kernel32.dll", b"GlobalMemoryStatusEx") else { return };
        READY.store(1, Ordering::Release);
        let mut old = 0u32;
        if VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old) == 0 {
            return;
        }
        let hook: unsafe extern "system" fn(*mut MemoryStatusEx) -> i32 = steady_global_memory_status_ex;
        core::ptr::write_volatile(slot, hook as *const () as usize);
        VirtualProtect(slot.cast(), 8, old, &mut old);
    }
}

/// The deduplication memory xtool takes, as a share of RAM in percent: its last `-dm<N>p` (or
/// `%`) on the command line, 75 without one. None for an absolute size or anything unclear
/// (left alone: an absolute -dm follows xtool's working set, not these figures).
fn dedup_percent(line: &[u16]) -> Option<u64> {
    let mut pct = Some(75);
    for word in line.split(|&c| c == b' ' as u16 || c == b'\t' as u16) {
        let Some(rest) = word.strip_prefix(&[b'-' as u16, b'd' as u16, b'm' as u16][..]) else { continue };
        let digits = rest.iter().take_while(|&&c| (b'0' as u16..=b'9' as u16).contains(&c)).count();
        let suffix = &rest[digits..];
        let value = rest[..digits].iter().fold(0u64, |v, &c| v.saturating_mul(10).saturating_add((c - b'0' as u16) as u64));
        pct = (digits > 0 && (suffix == [b'p' as u16] || suffix == [b'%' as u16])).then_some(value);
    }
    pct
}

/// The free memory to report: unchanged, unless xtool's budget from these figures,
/// C = min(pct% x total - used, free - 1 GiB), would land below 2 GiB; then at most 1 GiB,
/// which makes it 0.
fn avoid_small_budget(total: u64, avail: u64, pct: u64) -> u64 {
    const GIB: u64 = 1 << 30;
    let share = (total / 100).saturating_mul(pct.min(100));
    let used = total.saturating_sub(avail);
    let budget = share.saturating_sub(used).min(avail.saturating_sub(GIB));
    // (Also at 0: xtool rounds its share differently, so a plugin 0 can be a few bytes for it.)
    if budget < 2 * GIB { avail.min(GIB) } else { avail }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn dedup_share_from_the_command_line() {
        assert_eq!(dedup_percent(&w(r"rtool.exe decode -t100p - - ")), Some(75));
        assert_eq!(dedup_percent(&w(r"xtool.exe decode -t100p -dm50p - -")), Some(50));
        assert_eq!(dedup_percent(&w(r"xtool.exe decode -dm10% - -")), Some(10));
        assert_eq!(dedup_percent(&w(r"xtool.exe decode -dm512mb - -")), None);
    }

    #[test]
    fn small_budgets_become_none() {
        const GIB: u64 = 1 << 30;
        // 32 GB PC, 75%: 24 GB share. Little used: a big budget, left alone.
        assert_eq!(avoid_small_budget(32 * GIB, 30 * GIB, 75), 30 * GIB);
        // 23 GB used: a 1 GB budget, which can fill and grow: report 1 GiB free (budget 0).
        assert_eq!(avoid_small_budget(32 * GIB, 9 * GIB, 75), GIB);
        // More used than the share: the budget is 0, which xtool may round to a few bytes:
        // reporting 1 GiB free keeps its store at 0.
        assert_eq!(avoid_small_budget(32 * GIB, 6 * GIB, 75), GIB);
        // 16 GB PC, -dm50p, 7 GB used: an 8 GB share leaves a 1 GB budget: small too.
        assert_eq!(avoid_small_budget(16 * GIB, 9 * GIB, 50), GIB);
    }
}

/// The import address table entry of `dll!function` in the PE32+ image at `base`.
unsafe fn import_slot(base: *mut u8, dll: &[u8], function: &[u8]) -> Option<*mut usize> {
    unsafe {
        let u16_at = |off: usize| core::ptr::read_unaligned(base.add(off) as *const u16);
        let u32_at = |off: usize| core::ptr::read_unaligned(base.add(off) as *const u32);
        let u64_at = |off: usize| core::ptr::read_unaligned(base.add(off) as *const u64);
        let c_str_eq = |off: usize, want: &[u8]| {
            for (i, &w) in want.iter().enumerate() {
                if !(*base.add(off + i)).eq_ignore_ascii_case(&w) {
                    return false;
                }
            }
            *base.add(off + want.len()) == 0
        };
        if u16_at(0) != 0x5A4D {
            return None; // "MZ"
        }
        let nt = u32_at(0x3C) as usize;
        if u32_at(nt) != 0x0000_4550 || u16_at(nt + 24) != 0x20B {
            return None; // "PE\0\0", PE32+
        }
        let imports = u32_at(nt + 24 + 112 + 8) as usize;
        if imports == 0 {
            return None;
        }
        let mut desc = imports;
        loop {
            let (names, name, iat) = (u32_at(desc) as usize, u32_at(desc + 12) as usize, u32_at(desc + 16) as usize);
            if name == 0 {
                return None;
            }
            if c_str_eq(name, dll) {
                let names = if names != 0 { names } else { iat };
                let mut i = 0;
                loop {
                    let entry = u64_at(names + 8 * i);
                    if entry == 0 {
                        break;
                    }
                    // By name (not by ordinal): IMAGE_IMPORT_BY_NAME = hint (2 bytes), name.
                    if entry >> 63 == 0 && c_str_eq(entry as usize + 2, function) {
                        return Some(base.add(iat + 8 * i) as *mut usize);
                    }
                    i += 1;
                }
            }
            desc += 20;
        }
    }
}
