//! Keeping third-party codecs on a leash (Windows):
//!  - `kill_descendants_on_exit`: put this process in a kill-on-close job object, so every
//!    process it starts (and *their* children, e.g. a CLS proxy's 64-bit helper) dies with it.
//!    No orphaned codec processes after a failure, crash or Ctrl+C.
//!  - `dismiss_codec_dialogs`: some codecs report errors with a modal MessageBox and then block
//!    until someone clicks OK. This finds such dialogs owned by our descendant processes,
//!    returns their text, and presses OK so the codec's error path continues.

#[cfg(windows)]
mod imp {
    use std::collections::HashSet;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM};
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BM_CLICK, EnumChildWindows, EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible, PostMessageW, WM_CLOSE,
    };

    static JOB: Mutex<usize> = Mutex::new(0);
    static SEEN: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

    /// The CLS-srep proxy DLL learns the shared-memory name prefix its 64-bit helper uses by
    /// reading a mapping called "Init_MapFile_". The helper always creates its buffers as
    /// "Global\Read_MapFile_…" but never creates Init_MapFile_ itself: the repack's installer
    /// does. Without it the DLL looks for "Read_MapFile_…" and fails ("Can't open read file
    /// mapping!"). Provide it the same way. (The handle stays open for the process lifetime.)
    pub fn provide_cls_proxy_init() -> bool {
        use windows_sys::Win32::System::Memory::{CreateFileMappingA, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE};
        unsafe {
            let h = CreateFileMappingA(
                -1isize as HANDLE, // INVALID_HANDLE_VALUE: pagefile-backed
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                256,
                c"Init_MapFile_".as_ptr() as *const u8,
            );
            if h.is_null() {
                return false;
            }
            let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 256);
            if view.Value.is_null() {
                return false;
            }
            let prefix = b"Global\\\0";
            std::ptr::copy_nonoverlapping(prefix.as_ptr(), view.Value as *mut u8, prefix.len());
            true
        }
    }

    /// True when running with an elevated (administrator) token.
    pub fn is_elevated() -> bool {
        use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
        use windows_sys::Win32::System::Threading::OpenProcessToken;
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut e as *mut _ as *mut core::ffi::c_void,
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            windows_sys::Win32::Foundation::CloseHandle(token);
            ok != 0 && e.TokenIsElevated != 0
        }
    }

    pub fn kill_descendants_on_exit() -> bool {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return false;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(job, GetCurrentProcess()) != 0;
            if ok {
                // The handle stays open for the life of the process; closing it at exit kills the job.
                *JOB.lock().unwrap() = job as usize;
            }
            ok
        }
    }

    /// Codec helpers are often started by codec DLLs/programs, not by us (e.g. a CLS proxy's
    /// 64-bit helper), so they miss the tuning applied at spawn. Apply it to every process in
    /// our job that hasn't had it yet: above-normal priority and no EcoQoS power throttling.
    pub fn tune_descendants(done: &mut HashSet<u32>) {
        use windows_sys::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, ProcessPowerThrottling,
            SetPriorityClass, SetProcessInformation,
        };
        const PROCESS_SET_LIMITED_INFORMATION: u32 = 0x2000;
        let fresh: Vec<u32> = job_pids().into_iter().filter(|p| done.insert(*p)).collect();
        if fresh.is_empty() {
            return;
        }
        // A helper started by a codec process (a CLS proxy's 64-bit worker) belongs to that
        // codec's stage: find it through its parents.
        let parents = if crate::perf::pipeline_mode() == crate::perf::Pipeline::Flat { Default::default() } else { parent_map() };
        let rank_of = |mut pid: u32| -> Option<usize> {
            for _ in 0..8 {
                if let Some(r) = crate::perf::stage_rank(pid) {
                    return Some(r);
                }
                pid = *parents.get(&pid)?;
            }
            None
        };
        for pid in fresh {
            unsafe {
                let h = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_LIMITED_INFORMATION, 0, pid);
                if h.is_null() {
                    continue;
                }
                if let Some(rank) = rank_of(pid) {
                    crate::perf::apply_rank(h, rank);
                    windows_sys::Win32::Foundation::CloseHandle(h);
                    continue;
                }
                SetPriorityClass(h, ABOVE_NORMAL_PRIORITY_CLASS);
                let state = PROCESS_POWER_THROTTLING_STATE {
                    Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                    ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                    StateMask: 0,
                };
                SetProcessInformation(
                    h,
                    ProcessPowerThrottling,
                    &state as *const _ as *const core::ffi::c_void,
                    size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
                );
                windows_sys::Win32::Foundation::CloseHandle(h);
            }
        }
    }

    /// pid -> parent pid for every process.
    fn parent_map() -> std::collections::HashMap<u32, u32> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        let mut out = std::collections::HashMap::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == -1 {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut e) != 0 {
                loop {
                    out.insert(e.th32ProcessID, e.th32ParentProcessID);
                    if Process32NextW(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            windows_sys::Win32::Foundation::CloseHandle(snap);
        }
        out
    }

    fn job_pids() -> HashSet<u32> {
        let job = *JOB.lock().unwrap() as HANDLE;
        if job.is_null() {
            return HashSet::new();
        }
        let mut buf = vec![0usize; 2 + 4096];
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr().cast(),
                (buf.len() * size_of::<usize>()) as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return HashSet::new();
        }
        let bytes = buf.as_ptr() as *const u8;
        let n = unsafe { *(bytes.add(4) as *const u32) } as usize;
        let ids = unsafe { std::slice::from_raw_parts(bytes.add(8) as *const usize, n.min(4096)) };
        let me = std::process::id();
        ids.iter().map(|&p| p as u32).filter(|&p| p != me).collect()
    }

    fn text_of(h: HWND) -> String {
        let mut b = [0u16; 512];
        let n = unsafe { GetWindowTextW(h, b.as_mut_ptr(), b.len() as i32) };
        String::from_utf16_lossy(&b[..n.max(0) as usize])
    }

    fn class_of(h: HWND) -> String {
        let mut b = [0u16; 64];
        let n = unsafe { GetClassNameW(h, b.as_mut_ptr(), b.len() as i32) };
        String::from_utf16_lossy(&b[..n.max(0) as usize])
    }

    struct Scan {
        pids: HashSet<u32>,
        found: Vec<(HWND, u32)>,
    }

    unsafe extern "system" fn on_window(h: HWND, l: LPARAM) -> BOOL {
        let scan = unsafe { &mut *(l as *mut Scan) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        if scan.pids.contains(&pid) && unsafe { IsWindowVisible(h) } != 0 && class_of(h) == "#32770" {
            scan.found.push((h, pid));
        }
        1
    }

    struct Children {
        texts: Vec<String>,
        buttons: Vec<HWND>,
    }

    unsafe extern "system" fn on_child(h: HWND, l: LPARAM) -> BOOL {
        let c = unsafe { &mut *(l as *mut Children) };
        match class_of(h).as_str() {
            "Static" => {
                let t = text_of(h);
                if !t.trim().is_empty() {
                    c.texts.push(t.trim().to_string());
                }
            }
            "Button" => c.buttons.push(h),
            _ => {}
        }
        1
    }

    pub fn dismiss_codec_dialogs() -> Vec<String> {
        let pids = job_pids();
        if pids.is_empty() {
            return Vec::new();
        }
        let mut scan = Scan { pids, found: Vec::new() };
        unsafe { EnumWindows(Some(on_window), &mut scan as *mut Scan as LPARAM) };
        let mut out = Vec::new();
        let mut seen = SEEN.lock().unwrap();
        let seen = seen.get_or_insert_with(HashSet::new);
        for (h, pid) in scan.found {
            let mut c = Children { texts: Vec::new(), buttons: Vec::new() };
            unsafe { EnumChildWindows(h, Some(on_child), &mut c as *mut Children as LPARAM) };
            // A posted WM_COMMAND/IDOK does not close a MessageBox; clicking its (first) button
            // or WM_CLOSE does (verified). Do both.
            if let Some(&b) = c.buttons.first() {
                unsafe { PostMessageW(b, BM_CLICK, 0, 0) };
            }
            unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
            if seen.insert(h as usize) {
                out.push(format!("{} (pid {pid}): {}", text_of(h), c.texts.join(" ")));
            }
        }
        out
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(Some(0)).collect()
    }

    /// Free bytes on the volume holding `path` (or its nearest existing ancestor).
    pub fn free_space(path: &std::path::Path) -> Option<u64> {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let abs = std::path::absolute(path).ok()?;
        let existing = abs.ancestors().find(|p| p.exists())?;
        let w = wide(existing.as_os_str());
        let mut free = 0u64;
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }

    /// Starts this program again with administrator rights (one UAC prompt), waits for it, and
    /// returns its exit code.
    pub fn relaunch_elevated(args: &[String]) -> anyhow::Result<i32> {
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
        use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
        let exe = std::env::current_exe()?;
        let quoted: Vec<String> = args.iter().map(|a| format!("\"{}\"", a.replace('"', "\\\""))).collect();
        let (verb, file, params) =
            (wide("runas".as_ref()), wide(exe.as_os_str()), wide(std::ffi::OsStr::new(&quoted.join(" "))));
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = 1; // SW_SHOWNORMAL
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            anyhow::bail!("administrator rights were not granted");
        }
        let mut code = 1u32;
        unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            GetExitCodeProcess(info.hProcess, &mut code);
            windows_sys::Win32::Foundation::CloseHandle(info.hProcess);
        }
        Ok(code as i32)
    }

    /// One of the machine-wide "heavy job" slots, held by the worker thread that runs the job.
    /// Named mutexes are shared by every arc-unpack process in the session (CLI, GUI,
    /// benchmarks), so two installs can't both run memory-hungry codecs at once; Windows
    /// releases an abandoned one if its process dies.
    pub struct HeavySlot(HANDLE);

    impl Drop for HeavySlot {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Threading::ReleaseMutex;
            unsafe {
                ReleaseMutex(self.0);
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    /// Physical memory: (available, total) bytes.
    pub fn memory() -> (u64, u64) {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 { (m.ullAvailPhys, m.ullTotalPhys) } else { (0, 0) }
    }

    /// Takes a free slot among `limit` without waiting, or None if all are busy. The first slot
    /// is always available to take; the others only while plenty of RAM is free (at least a
    /// quarter of it, and 8 GB), so a second heavy job runs only when it fits.
    pub fn try_heavy_slot(limit: usize) -> Option<HeavySlot> {
        use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        for n in 0..limit {
            if n >= 1 {
                let (available, total) = memory();
                if available < (total / 4).max(8 << 30) {
                    break;
                }
            }
            let name = wide(std::ffi::OsStr::new(&format!("Local\\arc-unpack-heavy-{n}")));
            let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if h.is_null() {
                continue;
            }
            match unsafe { WaitForSingleObject(h, 0) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => return Some(HeavySlot(h)),
                _ => unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(h);
                },
            }
        }
        None
    }

    /// Codec start-up lock. CLS proxy DLLs claim a numbered IPC channel by probing which
    /// numbers are free; two starting at the same moment can claim the same one and read each
    /// other's data. Holding this (session-wide) until the codec is running prevents that.
    /// It's a semaphore, so any thread may release it (the codec calls back on its own threads);
    /// waiting is capped so a crashed holder can't block everyone.
    pub struct InitLock(HANDLE);

    impl InitLock {
        pub fn acquire() -> Option<InitLock> {
            use windows_sys::Win32::System::Threading::{CreateSemaphoreW, WaitForSingleObject};
            let name = wide(std::ffi::OsStr::new("Local\\arc-unpack-codec-init"));
            let h = unsafe { CreateSemaphoreW(std::ptr::null(), 1, 1, name.as_ptr()) };
            if h.is_null() {
                return None;
            }
            unsafe { WaitForSingleObject(h, 60_000) };
            Some(InitLock(h))
        }
    }

    impl Drop for InitLock {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Threading::ReleaseSemaphore;
            unsafe {
                ReleaseSemaphore(self.0, 1, std::ptr::null_mut());
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    // The lock is created and released on different threads by design.
    unsafe impl Send for InitLock {}

    /// (system CPU busy fraction since the previous call, RAM used bytes, RAM total bytes)
    pub fn system_usage() -> (f64, u64, u64) {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        static LAST: Mutex<(u64, u64)> = Mutex::new((0, 0));
        let ft = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut idle, mut kernel, mut user) = (z, z, z);
        let mut cpu = 0.0;
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } != 0 {
            let (idle, total) = (ft(idle), ft(kernel) + ft(user)); // kernel time includes idle
            let mut last = LAST.lock().unwrap();
            let (d_idle, d_total) = (idle.saturating_sub(last.0), total.saturating_sub(last.1));
            if last.1 != 0 && d_total > 0 {
                cpu = 1.0 - d_idle as f64 / d_total as f64;
            }
            *last = (idle, total);
        }
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        let (used, total) = if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
            (m.ullTotalPhys - m.ullAvailPhys, m.ullTotalPhys)
        } else {
            (0, 0)
        };
        (cpu.clamp(0.0, 1.0), used, total)
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct HeavySlot;
    pub fn try_heavy_slot(_: usize) -> Option<HeavySlot> {
        Some(HeavySlot)
    }
    pub fn memory() -> (u64, u64) {
        (0, 0)
    }
    pub struct InitLock;
    impl InitLock {
        pub fn acquire() -> Option<InitLock> {
            None
        }
    }
    pub fn free_space(_: &std::path::Path) -> Option<u64> {
        None
    }
    pub fn relaunch_elevated(_: &[String]) -> anyhow::Result<i32> {
        anyhow::bail!("elevation is only supported on Windows")
    }
    pub fn system_usage() -> (f64, u64, u64) {
        (0.0, 0, 0)
    }
    pub fn tune_descendants(_: &mut std::collections::HashSet<u32>) {}
    pub fn kill_descendants_on_exit() -> bool {
        false
    }
    pub fn provide_cls_proxy_init() -> bool {
        false
    }
    pub fn is_elevated() -> bool {
        false
    }
    pub fn dismiss_codec_dialogs() -> Vec<String> {
        Vec::new()
    }
}

pub use imp::*;

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn heavy_slots_are_exclusive_across_threads() {
        // Unique limit offset so parallel test runs don't collide: use slot space 0..1 here.
        let held = try_heavy_slot(1).expect("slot 0 free");
        let other = std::thread::spawn(|| try_heavy_slot(1).is_some()).join().unwrap();
        assert!(!other, "a second holder got the same slot");
        drop(held);
        let again = std::thread::spawn(|| try_heavy_slot(1).is_some()).join().unwrap();
        assert!(again, "slot not released");
    }

    #[test]
    fn init_lock_serializes() {
        let first = InitLock::acquire().unwrap();
        let t = std::time::Instant::now();
        let waiter = std::thread::spawn(move || {
            let _l = InitLock::acquire();
            t.elapsed()
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(first); // released from a different thread than any waiter: semaphores allow that
        let waited = waiter.join().unwrap();
        assert!(waited >= std::time::Duration::from_millis(250), "second acquire didn't wait ({waited:?})");
    }
}
