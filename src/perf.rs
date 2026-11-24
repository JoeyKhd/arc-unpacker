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
}

pub use imp::*;
