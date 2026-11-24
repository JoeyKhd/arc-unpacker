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
}

#[cfg(not(windows))]
mod imp {
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
