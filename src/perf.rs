//! Windows scheduling tweaks for codec processes:
//!  - opt out of EcoQoS ("power throttling"), so Windows 11 doesn't park a background
//!    decoder on efficiency cores at reduced clocks;
//!  - optionally restrict a process to the fastest core class (P-cores on hybrid CPUs);
//!  - read back CPU time per process, so the summary can show which stage is the bottleneck.

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
    use windows_sys::Win32::System::SystemInformation::{
        CpuSetInformation, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling,
        SetProcessDefaultCpuSets, SetProcessInformation,
    };

    pub use windows_sys::Win32::System::Threading::ABOVE_NORMAL_PRIORITY_CLASS as PRIORITY_FLAGS;

    fn no_throttle(h: HANDLE) -> bool {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0, // control it, and turn it off
        };
        unsafe {
            SetProcessInformation(
                h,
                ProcessPowerThrottling,
                &state as *const _ as *const core::ffi::c_void,
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            ) != 0
        }
    }

    pub fn tune_self() {
        unsafe { no_throttle(GetCurrentProcess()) };
    }

    /// Applies the pipeline policy to one codec process: `rank` 0 is the first decode stage.
    pub fn apply_rank(h: HANDLE, rank: usize) {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, SetPriorityClass,
        };
        no_throttle(h);
        let class = match (super::pipeline_mode(), rank) {
            (super::Pipeline::Flat, _) => PRIORITY_FLAGS,
            (super::Pipeline::Low, 0) => PRIORITY_FLAGS,
            (super::Pipeline::Low, 1) => NORMAL_PRIORITY_CLASS,
            (super::Pipeline::Low, _) => BELOW_NORMAL_PRIORITY_CLASS,
            (_, 0) => HIGH_PRIORITY_CLASS,
            (_, 1) => PRIORITY_FLAGS,
            (_, _) => NORMAL_PRIORITY_CLASS,
        };
        unsafe { SetPriorityClass(h, class) };
        if super::pipeline_mode() == super::Pipeline::HighP && rank == 0 {
            let ids = performance_cpu_sets();
            if !ids.is_empty() {
                unsafe { SetProcessDefaultCpuSets(h, ids.as_ptr(), ids.len() as u32) };
            }
        }
    }

    pub fn tune_child(child: &std::process::Child, performance_cores_only: bool) {
        let h = child.as_raw_handle() as HANDLE;
        no_throttle(h);
        if performance_cores_only {
            let ids = performance_cpu_sets();
            if !ids.is_empty() {
                unsafe { SetProcessDefaultCpuSets(h, ids.as_ptr(), ids.len() as u32) };
            }
        }
    }

    /// CPU set ids of the highest efficiency class (P-cores). Empty if the CPU isn't hybrid.
    pub fn performance_cpu_sets() -> Vec<u32> {
        let mut len = 0u32;
        unsafe {
            GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, std::ptr::null_mut(), 0);
        }
        if len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = unsafe {
            GetSystemCpuSetInformation(buf.as_mut_ptr().cast(), len, &mut len, std::ptr::null_mut(), 0)
        };
        if ok == 0 {
            return Vec::new();
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len as usize) };
        let mut sets = Vec::new(); // (id, efficiency class)
        let mut off = 0usize;
        while off + 8 <= bytes.len() {
            let info = unsafe { &*(bytes.as_ptr().add(off) as *const SYSTEM_CPU_SET_INFORMATION) };
            if info.Size == 0 {
                break;
            }
            if info.Type == CpuSetInformation {
                let cs = unsafe { info.Anonymous.CpuSet };
                sets.push((cs.Id, cs.EfficiencyClass));
            }
            off += info.Size as usize;
        }
        let max = sets.iter().map(|s| s.1).max().unwrap_or(0);
        let min = sets.iter().map(|s| s.1).min().unwrap_or(0);
        if max == min {
            return Vec::new();
        }
        sets.into_iter().filter(|s| s.1 == max).map(|s| s.0).collect()
    }

    /// User + kernel CPU seconds consumed by a (possibly exited) child.
    pub fn cpu_seconds(child: &std::process::Child) -> Option<f64> {
        let h = child.as_raw_handle() as HANDLE;
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z, z, z, z);
        let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
        if ok == 0 {
            return None;
        }
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        Some(t(k) + t(u))
    }

    /// Adds up the CPU time of a codec process and everything it starts. A CLS proxy runs the
    /// real decoder as a child process (cls-magic2.dll -> cls-magic2_x64.exe), and unarc.dll
    /// runs external codecs (bcm, rzw, ...) as children, so the process alone shows next to
    /// nothing. Processes a job counts stay counted after they exit.
    pub struct CpuJob(HANDLE);
    // SAFETY: a job handle can be used from any thread.
    unsafe impl Send for CpuJob {}
    unsafe impl Sync for CpuJob {}

    impl CpuJob {
        /// None if the process can't be put in a job (then only its own time is known).
        /// Children it started before this call aren't counted; codecs start theirs later.
        pub fn track(child: &std::process::Child) -> Option<CpuJob> {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                // Nested inside any job the process is already in (Windows 8+).
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(CpuJob(job))
            }
        }

        /// User + kernel CPU seconds of every process that has been in the job.
        pub fn cpu_seconds(&self) -> Option<f64> {
            use windows_sys::Win32::System::JobObjects::{
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation, QueryInformationJobObject,
            };
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then(|| (info.TotalUserTime + info.TotalKernelTime) as f64 / 1e7)
        }
    }

    impl Drop for CpuJob {
        fn drop(&mut self) {
            // Closing only the handle: the processes keep running (no kill-on-close limit).
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

/// How the codec processes of one decode chain share the CPU. The first decode stage (the
/// entropy decoder reading the archive) is usually serial and sets the pace; later stages
/// (xtool and friends) are the thread-hungry ones. Because stages are connected by pipes with
/// back-pressure, favouring upstream can't starve downstream: a stage that is ahead just blocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pipeline {
    /// Every codec process above normal priority.
    Flat,
    /// Above normal / normal / below normal, by position.
    Low,
    /// High / above normal / normal, by position.
    High,
    /// `High`, and the first stage restricted to the fastest core class (P-cores).
    HighP,
}

/// The policy in use: `ARC_UNPACK_PIPELINE` = flat | low | high | high+p.
pub fn pipeline_mode() -> Pipeline {
    static MODE: std::sync::OnceLock<Pipeline> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("ARC_UNPACK_PIPELINE").unwrap_or_default().to_ascii_lowercase().as_str() {
        "low" => Pipeline::Low,
        "high" => Pipeline::High,
        "high+p" => Pipeline::HighP,
        _ => Pipeline::Flat,
    })
}

fn ranks() -> &'static std::sync::Mutex<std::collections::HashMap<u32, usize>> {
    static RANKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, usize>>> = std::sync::OnceLock::new();
    RANKS.get_or_init(Default::default)
}

/// Records that process `pid` runs decode stage number `rank` of its chain.
pub fn register_stage(pid: u32, rank: usize) {
    ranks().lock().unwrap().insert(pid, rank);
}

pub fn stage_rank(pid: u32) -> Option<usize> {
    ranks().lock().unwrap().get(&pid).copied()
}

#[cfg(not(windows))]
mod imp {
    pub const PRIORITY_FLAGS: u32 = 0;
    pub fn tune_self() {}
    pub fn tune_child(_: &std::process::Child, _: bool) {}
    pub fn performance_cpu_sets() -> Vec<u32> {
        Vec::new()
    }
    pub fn cpu_seconds(_: &std::process::Child) -> Option<f64> {
        None
    }
    pub struct CpuJob;
    impl CpuJob {
        pub fn track(_: &std::process::Child) -> Option<CpuJob> {
            None
        }
        pub fn cpu_seconds(&self) -> Option<f64> {
            None
        }
    }
}

pub use imp::*;
