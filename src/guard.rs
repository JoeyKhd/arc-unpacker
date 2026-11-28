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

    /// Makes this process's standard handles non-inheritable (they stay usable here). A codec
    /// that starts a helper with bInheritHandles=TRUE (cls-lollypop.dll) would otherwise hand it
    /// our pipes, and the stage after ours would only see the end of the stream once the
    /// helper is gone too. `stderr`: also stderr (unarc-host keeps it, for its external codecs'
    /// messages).
    pub fn keep_std_handles_private(stderr: bool) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        let mut handles = vec![std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle()];
        if stderr {
            handles.push(std::io::stderr().as_raw_handle());
        }
        for h in handles {
            if !h.is_null() {
                unsafe { SetHandleInformation(h as _, HANDLE_FLAG_INHERIT, 0) };
            }
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

    /// Bytes read and written so far by this process and everything it started, those that
    /// already exited included. None without the job (see `kill_descendants_on_exit`).
    pub fn job_io_bytes() -> Option<u64> {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, JobObjectBasicAndIoAccountingInformation,
        };
        let job = *JOB.lock().unwrap() as HANDLE;
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicAndIoAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then(|| info.IoInfo.ReadTransferCount + info.IoInfo.WriteTransferCount)
    }

    /// Names of the processes this one started that still run (e.g. "cmd.exe, countdown.exe").
    pub fn running_programs() -> Vec<String> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
        };
        let pids = job_pids();
        let mut names = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == -1 {
                return names;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut e) != 0 {
                loop {
                    if pids.contains(&e.th32ProcessID) {
                        let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                        let name = String::from_utf16_lossy(&e.szExeFile[..n]);
                        if !name.eq_ignore_ascii_case("conhost.exe") && !names.contains(&name) {
                            names.push(name);
                        }
                    }
                    if Process32NextW(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            windows_sys::Win32::Foundation::CloseHandle(snap);
        }
        names
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
        // (A bare file name's parent is "": the current folder.)
        let path = if path.as_os_str().is_empty() { std::path::Path::new(".") } else { path };
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
    /// The semaphore, and whether this holder took its count (a wait that timed out didn't).
    pub struct InitLock(HANDLE, bool);

    impl InitLock {
        pub fn acquire() -> Option<InitLock> {
            acquire_within(60_000)
        }

        /// Whether this holder has the lock (false after a timed-out wait).
        pub fn held(&self) -> bool {
            self.1
        }
    }

    pub fn acquire_within(ms: u32) -> Option<InitLock> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{CreateSemaphoreW, WaitForSingleObject};
        let name = wide(std::ffi::OsStr::new("Local\\arc-unpack-codec-init"));
        let h = unsafe { CreateSemaphoreW(std::ptr::null(), 1, 1, name.as_ptr()) };
        if h.is_null() {
            return None;
        }
        let took = unsafe { WaitForSingleObject(h, ms) } == WAIT_OBJECT_0;
        if !took {
            // A holder that never let go (a stuck codec start): go on without the lock rather
            // than wait forever, and don't give back a count this holder never took, which
            // would let a third instance in while the first still holds it.
            eprintln!("arc-unpack: waited {} s for another codec to start; starting without the lock", ms / 1000);
        }
        Some(InitLock(h, took))
    }

    impl Drop for InitLock {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Threading::ReleaseSemaphore;
            unsafe {
                if self.1 {
                    ReleaseSemaphore(self.0, 1, std::ptr::null_mut());
                }
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    // The lock is created and released on different threads by design.
    unsafe impl Send for InitLock {}

    std::thread_local! {
        /// The start-up lock this thread took at a CLS proxy's first channel probe.
        static CLAIM: std::cell::RefCell<Option<InitLock>> = const { std::cell::RefCell::new(None) };
    }

    /// Whether a C string starts with "IOEvent_": the event a CLS proxy DLL probes for
    /// (`OpenEventA`, numbers 00-99) and then creates (`CreateEventA`) to claim its channel.
    fn is_channel_event(name: *const u8) -> bool {
        if name.is_null() {
            return false;
        }
        b"IOEvent_".iter().enumerate().all(|(i, &c)| unsafe { *name.add(i) } == c)
    }

    unsafe extern "system" fn claim_open_event_a(access: u32, inherit: BOOL, name: *const u8) -> HANDLE {
        if is_channel_event(name) {
            CLAIM.with(|c| {
                let mut c = c.borrow_mut();
                if c.is_none() {
                    *c = acquire_within(60_000);
                }
            });
        }
        unsafe { windows_sys::Win32::System::Threading::OpenEventA(access, inherit, name) }
    }

    unsafe extern "system" fn claim_create_event_a(
        sa: *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
        manual: BOOL,
        initial: BOOL,
        name: *const u8,
    ) -> HANDLE {
        let h = unsafe { windows_sys::Win32::System::Threading::CreateEventA(sa, manual, initial, name) };
        if is_channel_event(name) {
            CLAIM.with(|c| drop(c.borrow_mut().take()));
        }
        h
    }

    /// Hooks a loaded CLS proxy DLL's channel claim, so it takes `InitLock` only from its first
    /// `OpenEventA("IOEvent_...")` probe to the `CreateEventA` that claims the number: its
    /// import table entries for kernel32's OpenEventA and CreateEventA are pointed at hooks that
    /// do that around the real calls. For hosts where the DLL is loaded by someone else's code
    /// (unarc.dll), which can't hold the lock around the codec's start. True when both were found.
    ///
    /// # Safety
    /// `module` must be the base of a PE image loaded in this process.
    pub unsafe fn hook_channel_claims(module: *mut std::ffi::c_void) -> bool {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        use windows_sys::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
        let k32 = unsafe { GetModuleHandleW(wide(std::ffi::OsStr::new("kernel32.dll")).as_ptr()) };
        if k32.is_null() || module.is_null() {
            return false;
        }
        let real = |n: &std::ffi::CStr| unsafe { GetProcAddress(k32, n.as_ptr().cast()) }.map(|f| f as usize);
        let wanted: [(&[u8], Option<usize>, usize); 2] = [
            (b"OpenEventA", real(c"OpenEventA"), claim_open_event_a as unsafe extern "system" fn(u32, BOOL, *const u8) -> HANDLE as usize),
            (
                b"CreateEventA",
                real(c"CreateEventA"),
                claim_create_event_a as unsafe extern "system" fn(*const windows_sys::Win32::Security::SECURITY_ATTRIBUTES, BOOL, BOOL, *const u8) -> HANDLE
                    as usize,
            ),
        ];
        let mut found = [false; 2];
        for (slot, which) in unsafe { crate::guard::kernel32_slots(module.cast(), &wanted.map(|w| (w.0, w.1))) } {
            let mut old: PAGE_PROTECTION_FLAGS = 0;
            unsafe {
                if VirtualProtect(slot.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old) == 0 {
                    continue;
                }
                slot.write_unaligned(wanted[which].2);
                VirtualProtect(slot.cast(), size_of::<usize>(), old, &mut old);
            }
            found[which] = true;
        }
        found == [true, true]
    }

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
    pub fn job_io_bytes() -> Option<u64> {
        None
    }
    pub fn running_programs() -> Vec<String> {
        Vec::new()
    }
    pub fn kill_descendants_on_exit() -> bool {
        false
    }
    pub fn keep_std_handles_private(_: bool) {}
    pub fn provide_cls_proxy_init() -> bool {
        false
    }
    /// # Safety
    /// Nothing to do off Windows.
    pub unsafe fn hook_channel_claims(_: *mut std::ffi::c_void) -> bool {
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

/// The import address table slots of kernel32.dll's `wanted` functions in a PE image loaded in
/// this process (`base`): found by name in the import names, or, for an entry without one, by
/// the slot already holding the function's address (`wanted.1`). (slot, index into `wanted`).
///
/// # Safety
/// `base` must be the base of a PE image mapped in this process.
pub unsafe fn kernel32_slots(base: *const u8, wanted: &[(&[u8], Option<usize>)]) -> Vec<(*mut usize, usize)> {
    let rd32 = |off: usize| unsafe { (base.add(off) as *const u32).read_unaligned() } as usize;
    let rd16 = |off: usize| unsafe { (base.add(off) as *const u16).read_unaligned() } as usize;
    let mut out = Vec::new();
    unsafe {
        if rd16(0) != 0x5A4D {
            return out;
        }
        let nt = rd32(0x3C);
        if rd32(nt) != 0x4550 {
            return out;
        }
        let opt = nt + 24;
        let ptr = size_of::<usize>();
        let dd = opt + match (rd16(opt), ptr) {
            (0x10B, 4) => 96,
            (0x20B, 8) => 112,
            _ => return out,
        };
        let imports = rd32(dd + 8);
        if imports == 0 {
            return out;
        }
        let cstr = |off: usize| std::ffi::CStr::from_ptr(base.add(off).cast()).to_bytes();
        let mut d = imports;
        loop {
            let (names, name, iat) = (rd32(d), rd32(d + 12), rd32(d + 16));
            if name == 0 && iat == 0 {
                break;
            }
            if cstr(name).eq_ignore_ascii_case(b"kernel32.dll") {
                for i in 0.. {
                    let slot = base.add(iat + i * ptr) as *mut usize;
                    let value = slot.read_unaligned();
                    if value == 0 {
                        break;
                    }
                    let by_name = if names != 0 {
                        let entry = (base.add(names + i * ptr) as *const usize).read_unaligned();
                        let ordinal = entry & (1usize << (ptr * 8 - 1)) != 0;
                        (!ordinal).then(|| cstr(entry + 2))
                    } else {
                        None
                    };
                    let which = wanted.iter().position(|(n, addr)| match by_name {
                        Some(found) => found == *n,
                        None => *addr == Some(value),
                    });
                    if let Some(w) = which {
                        out.push((slot, w));
                    }
                }
            }
            d += 20;
        }
    }
    out
}

/// Whether a DLL file imports kernel32's OpenEventA and CreateEventA by name, the calls
/// `hook_channel_claims` hooks (packed DLLs, which resolve their imports themselves, don't).
pub fn imports_channel_claims(dll: &std::path::Path) -> bool {
    let Ok(b) = std::fs::read(dll) else { return false };
    let u16_at = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize);
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()) as usize);
    let u64_at = |o: usize| b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    (|| -> Option<bool> {
        let nt = u32_at(0x3C)?;
        if u32_at(nt)? != 0x4550 {
            return Some(false);
        }
        let (nsec, opt) = (u16_at(nt + 6)?, nt + 24);
        let pe32 = u16_at(opt)? == 0x10B;
        let imports = u32_at(opt + if pe32 { 96 } else { 112 } + 8)?;
        let secs = opt + u16_at(nt + 20)?;
        // A virtual address in the file, through the section table.
        let at = |rva: usize| -> Option<usize> {
            (0..nsec).find_map(|i| {
                let s = secs + 40 * i;
                let (vsize, va, rsize, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 16)?, u32_at(s + 20)?);
                (va <= rva && rva < va + vsize.max(rsize)).then_some(rva - va + raw)
            })
        };
        let cstr = |o: usize| -> Option<&[u8]> {
            let s = b.get(o..)?;
            Some(&s[..s.iter().position(|&c| c == 0)?])
        };
        let (mut open, mut create) = (false, false);
        let mut d = at(imports)?;
        loop {
            let (names, name) = (u32_at(d)?, u32_at(d + 12)?);
            if name == 0 {
                break;
            }
            if cstr(at(name)?)?.eq_ignore_ascii_case(b"kernel32.dll") && names != 0 {
                let mut t = at(names)?;
                loop {
                    let (entry, ordinal) = if pe32 {
                        let v = u32_at(t)? as u64;
                        t += 4;
                        (v, v & 0x8000_0000 != 0)
                    } else {
                        let v = u64_at(t)?;
                        t += 8;
                        (v, v >> 63 != 0)
                    };
                    if entry == 0 {
                        break;
                    }
                    if !ordinal && let Some(n) = at(entry as usize & 0x7FFF_FFFF).and_then(|o| cstr(o + 2)) {
                        open |= n == b"OpenEventA";
                        create |= n == b"CreateEventA";
                    }
                }
            }
            d += 20;
        }
        Some(open && create)
    })()
    .unwrap_or(false)
}

/// Notices an install that stopped making progress: this process and everything it started
/// (codecs, the repack's tools, tools those start) read and wrote less than `STALL_BYTES` in
/// the time limit. A tool waiting for a file that never comes, a dialog nobody sees, a
/// deadlock: whatever the cause, nothing moves on disk.
pub struct StallCheck {
    limit: Option<std::time::Duration>,
    base: u64,
    since: std::time::Instant,
    next: std::time::Instant,
}

/// Less I/O than this counts as none: a batch file's waiting loop (cmd re-reading its lines,
/// a 1-second timer program) does a few hundred bytes a second.
pub const STALL_BYTES: u64 = 4 << 20;

impl StallCheck {
    /// `limit_secs` 0: never stalls. Also never without the job (the counts come from it).
    pub fn new(limit_secs: u64) -> StallCheck {
        let now = std::time::Instant::now();
        let limit = (limit_secs > 0 && job_io_bytes().is_some()).then(|| std::time::Duration::from_secs(limit_secs));
        StallCheck { limit, base: job_io_bytes().unwrap_or(0), since: now, next: now }
    }

    /// Call often (it looks once a second): true once nothing moved for the limit.
    pub fn stalled(&mut self) -> bool {
        let Some(limit) = self.limit else { return false };
        let now = std::time::Instant::now();
        if now < self.next {
            return false;
        }
        self.next = now + std::time::Duration::from_secs(1);
        let io = job_io_bytes().unwrap_or(self.base);
        if io.saturating_sub(self.base) >= STALL_BYTES {
            self.base = io;
            self.since = now;
        }
        now.duration_since(self.since) > limit
    }

    pub fn limit_secs(&self) -> u64 {
        self.limit.map(|l| l.as_secs()).unwrap_or(0)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn kernel32_import_slots_of_this_program() {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        std::hint::black_box(hook_channel_claims as unsafe fn(*mut std::ffi::c_void) -> bool);
        // (The hooks reference both functions, so this test program imports them.)
        let me = unsafe { GetModuleHandleW(std::ptr::null()) } as *const u8;
        let k32 = unsafe { GetModuleHandleW([107u16, 101, 114, 110, 101, 108, 51, 50, 0].as_ptr()) };
        let real = |n: &std::ffi::CStr| unsafe { GetProcAddress(k32, n.as_ptr().cast()) }.map(|f| f as usize);
        let slots = unsafe { kernel32_slots(me, &[(b"OpenEventA", real(c"OpenEventA")), (b"CreateEventA", real(c"CreateEventA"))]) };
        let found: Vec<usize> = slots.iter().map(|s| s.1).collect();
        assert!(found.contains(&0) && found.contains(&1), "{found:?}");
        // The slot holds the function the loader bound (kernel32's, or the one it forwards to).
        for (slot, _) in slots {
            assert_ne!(unsafe { slot.read_unaligned() }, 0);
        }
        assert!(imports_channel_claims(&std::env::current_exe().unwrap()));
        assert!(!imports_channel_claims(std::path::Path::new("no such file.dll")));
    }

    #[test]
    fn heavy_slots_are_exclusive_across_threads() {
        // Unique limit offset so parallel test runs don't collide: use slot space 0..1 here.
        // (The slots are machine-wide: an install running on this PC may hold it.)
        let Some(held) = try_heavy_slot(1) else {
            eprintln!("slot 0 is held by another arc-unpack on this PC; not tested");
            return;
        };
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

        // (Same test: the lock is one named semaphore.) A wait that times out holds nothing and
        // gives nothing back: the first holder still excludes everyone else.
        let first = InitLock::acquire().unwrap();
        assert!(first.held());
        let timed_out = acquire_within(100).unwrap();
        assert!(!timed_out.held());
        drop(timed_out);
        assert!(!acquire_within(100).unwrap().held(), "a timed-out holder released a count it never took");
        drop(first);
        assert!(acquire_within(100).unwrap().held());
    }

    /// A batch file's waiting loop counts as no progress; writing counts as progress. (Ignored:
    /// the counts cover the whole test process, so other tests' I/O would disturb it. Run it
    /// alone: `cargo test --lib stall_check -- --ignored`.)
    #[test]
    #[ignore]
    fn stall_check() {
        use std::time::{Duration, Instant};
        assert!(kill_descendants_on_exit());
        let mut stall = StallCheck::new(3);
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "for /l %i in (1,1,30) do @(ping -n 2 127.0.0.1 >nul & if exist nothing-here.txt exit)"])
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !stall.stalled() {
            assert!(started.elapsed() < Duration::from_secs(10), "a waiting loop wasn't seen as stalled");
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(started.elapsed() >= Duration::from_secs(3));
        assert!(running_programs().iter().any(|n| n.eq_ignore_ascii_case("cmd.exe")));
        let _ = child.kill();

        let dir = std::env::temp_dir().join(format!("arcunp-stall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut stall = StallCheck::new(3);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(6) {
            std::fs::write(dir.join("x"), vec![0u8; 2 << 20]).unwrap();
            assert!(!stall.stalled(), "writing was seen as a stall");
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
