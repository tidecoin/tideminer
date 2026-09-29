//! The only module with `unsafe`: thin OS bindings for thread placement and
//! CPU discovery. Everything else in the crate denies `unsafe_code`.
#![allow(unsafe_code)]

/// Pin the calling thread to one logical CPU. Returns false if the OS refused.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn pin_current_thread(cpu: usize) -> bool {
    if cpu >= libc::CPU_SETSIZE as usize {
        return false;
    }
    // SAFETY: cpu_set_t is plain data; zeroed is its empty set. CPU_SET writes
    // within the set for cpu < CPU_SETSIZE (checked above). sched_setaffinity(0)
    // targets the calling thread and reads exactly size_of::<cpu_set_t>() bytes.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// macOS has no hard affinity. Ask for the QoS class that favours the requested
/// core type instead: user-interactive for performance cores, utility for
/// efficiency cores (the scheduler keeps low QoS work off P-cores under load).
#[cfg(target_os = "macos")]
pub fn prefer_core_class(performance: bool) -> bool {
    let class = if performance {
        libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE
    } else {
        libc::qos_class_t::QOS_CLASS_UTILITY
    };
    // SAFETY: sets the calling thread's QoS; takes values, retains no pointers.
    unsafe { libc::pthread_set_qos_class_self_np(class, 0) == 0 }
}

/// Integer sysctl by name, e.g. `hw.perflevel0.logicalcpu`.
#[cfg(target_os = "macos")]
pub fn sysctl_u32(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>();
    // SAFETY: name is NUL-terminated; value/size describe a valid 4-byte buffer
    // that sysctlbyname fills and does not retain; no new value is written.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && size == std::mem::size_of::<u32>()).then_some(value)
}

/// String sysctl by name, e.g. `machdep.cpu.brand_string`.
#[cfg(target_os = "macos")]
pub fn sysctl_string(name: &str) -> Option<String> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut buffer = vec![0u8; 256];
    let mut size = buffer.len();
    // SAFETY: as above, with a 256-byte output buffer and its true length.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buffer.truncate(size);
    let text = String::from_utf8_lossy(&buffer);
    Some(text.trim_end_matches('\0').trim().to_owned())
}

/// Lower the calling thread's scheduling priority (Unix nice value, 0..=19).
#[cfg(unix)]
pub fn set_current_thread_nice(nice: i32) -> bool {
    // SAFETY: PRIO_PROCESS with who=0 on Linux applies to the calling thread
    // (threads are tasks); on macOS it applies to the process. Values only.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) == 0 }
}

/// Offset of local time from UTC in seconds at `unix_secs` (0 where unknown).
pub fn local_utc_offset(unix_secs: i64) -> i64 {
    #[cfg(unix)]
    // libc marks time_t deprecated on musl (its size changed in musl 1.2); on the
    // 64-bit targets built here it is 64 bits either way.
    #[allow(deprecated)]
    {
        let t: libc::time_t = unix_secs as libc::time_t;
        // SAFETY: localtime_r writes only into the provided, zeroed tm struct and
        // reads the time_t it is given; both live on this stack frame.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return 0;
            }
            tm.tm_gmtoff as i64
        }
    }
    #[cfg(windows)]
    {
        let _ = unix_secs;
        win::utc_offset_now()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = unix_secs;
        0
    }
}

/// Home directory of a user id from the password database (Unix).
#[cfg(unix)]
pub fn home_dir_of(uid: u32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0u8; 16 * 1024];
    // SAFETY: passwd is plain data filled by getpwuid_r; the strings it points to
    // live in `buffer`, which outlives every read below. `result` is either null
    // or points at `entry`.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            uid as libc::uid_t,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        );
        if rc != 0 || result.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(entry.pw_dir);
        Some(std::ffi::OsStr::from_bytes(dir.to_bytes()).into())
    }
}

/// True when running as root (Unix).
#[cfg(unix)]
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Keeps the system out of idle sleep while alive; the display may still sleep
/// (what `caffeinate -i` does). macOS idle-sleeps a few minutes after the display
/// turns off even with every core busy, silently stopping the miner.
#[cfg(target_os = "macos")]
pub struct KeepAwake(u32);

#[cfg(target_os = "macos")]
mod iokit {
    pub type CFStringRef = *const std::ffi::c_void;
    pub const UTF8: u32 = 0x0800_0100;
    pub const LEVEL_ON: u32 = 255;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFStringCreateWithCString(
            alloc: *const std::ffi::c_void,
            text: *const std::ffi::c_char,
            encoding: u32,
        ) -> CFStringRef;
        pub fn CFRelease(object: *const std::ffi::c_void);
        pub fn CFNumberGetValue(
            number: *const std::ffi::c_void,
            kind: isize,
            value: *mut std::ffi::c_void,
        ) -> bool;
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        pub fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        pub fn IOPMAssertionRelease(id: u32) -> i32;
        pub fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        pub fn IOServiceGetMatchingService(main_port: u32, matching: *mut std::ffi::c_void) -> u32;
        pub fn IORegistryEntryCreateCFProperty(
            entry: u32,
            key: CFStringRef,
            allocator: *const std::ffi::c_void,
            options: u32,
        ) -> *const std::ffi::c_void;
        pub fn IOObjectRelease(object: u32) -> i32;
    }
}

/// Hold a "prevent idle system sleep" power assertion named `reason` (shown by
/// `pmset -g assertions`). `None` if IOKit refused.
#[cfg(target_os = "macos")]
pub fn keep_awake(reason: &str) -> Option<KeepAwake> {
    use iokit::*;
    let reason = std::ffi::CString::new(reason).ok()?;
    // SAFETY: both strings are NUL-terminated and outlive the calls that read
    // them; each CFString created here is released exactly once below. `id` is a
    // plain out-parameter written only on success.
    unsafe {
        let kind = CFStringCreateWithCString(
            std::ptr::null(),
            c"PreventUserIdleSystemSleep".as_ptr(),
            UTF8,
        );
        let name = CFStringCreateWithCString(std::ptr::null(), reason.as_ptr(), UTF8);
        let mut id = 0u32;
        let rc = if kind.is_null() || name.is_null() {
            -1
        } else {
            IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut id)
        };
        for s in [kind, name] {
            if !s.is_null() {
                CFRelease(s);
            }
        }
        (rc == 0).then_some(KeepAwake(id))
    }
}

#[cfg(target_os = "macos")]
impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: releases the assertion this value created, once.
        unsafe {
            iokit::IOPMAssertionRelease(self.0);
        }
    }
}

/// GPU cores of the Apple GPU (`gpu-core-count` of its AGXAccelerator in the IO
/// registry), when readable.
#[cfg(target_os = "macos")]
pub fn gpu_core_count() -> Option<usize> {
    use iokit::*;
    const SINT64: isize = 4; // kCFNumberSInt64Type
    // SAFETY: IOServiceMatching returns a dictionary that IOServiceGetMatchingService
    // consumes; the service and the CF objects created here are released once; the
    // property is only read as a CFNumber into a local i64.
    unsafe {
        let matching = IOServiceMatching(c"AGXAccelerator".as_ptr());
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        let key = CFStringCreateWithCString(std::ptr::null(), c"gpu-core-count".as_ptr(), UTF8);
        let value = if key.is_null() {
            std::ptr::null()
        } else {
            IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0)
        };
        let mut cores: i64 = 0;
        let ok =
            !value.is_null() && CFNumberGetValue(value, SINT64, (&mut cores as *mut i64).cast());
        for object in [key, value] {
            if !object.is_null() {
                CFRelease(object);
            }
        }
        IOObjectRelease(service);
        (ok && cores > 0).then_some(cores as usize)
    }
}

// ---------------------------------------------------------------------------
// Windows (kernel32), declared directly: no bindings crate for five functions.

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub const ES_CONTINUOUS: u32 = 0x8000_0000;
    pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    pub const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

    #[repr(C)]
    pub struct SystemTime([u16; 8]);

    #[repr(C)]
    pub struct TimeZoneInformation {
        pub bias: i32,
        pub standard_name: [u16; 32],
        pub standard_date: SystemTime,
        pub standard_bias: i32,
        pub daylight_name: [u16; 32],
        pub daylight_date: SystemTime,
        pub daylight_bias: i32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn SetThreadExecutionState(flags: u32) -> u32;
        pub fn GetCurrentThread() -> *mut c_void;
        pub fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
        pub fn GetStdHandle(which: u32) -> *mut c_void;
        pub fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
        pub fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }

    /// Current offset of local time from UTC in seconds.
    pub fn utc_offset_now() -> i64 {
        // SAFETY: GetTimeZoneInformation fills the zeroed struct we pass and keeps
        // no pointer to it.
        unsafe {
            let mut info: TimeZoneInformation = std::mem::zeroed();
            let state = GetTimeZoneInformation(&mut info);
            if state == u32::MAX {
                return 0;
            }
            let mut bias = info.bias;
            if state == TIME_ZONE_ID_DAYLIGHT {
                bias += info.daylight_bias;
            }
            -i64::from(bias) * 60
        }
    }
}

/// Keeps Windows out of idle sleep while alive (the display may still turn off).
#[cfg(windows)]
pub struct KeepAwake(());

/// Ask Windows not to sleep while hashing: `ES_SYSTEM_REQUIRED`, held by the calling
/// thread until dropped. `reason` is not shown by Windows.
#[cfg(windows)]
pub fn keep_awake(reason: &str) -> Option<KeepAwake> {
    let _ = reason;
    // SAFETY: takes flags only; affects the calling thread's execution state.
    let previous =
        unsafe { win::SetThreadExecutionState(win::ES_CONTINUOUS | win::ES_SYSTEM_REQUIRED) };
    (previous != 0).then_some(KeepAwake(()))
}

#[cfg(windows)]
impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: clears the requirement this thread set; flags only.
        unsafe {
            win::SetThreadExecutionState(win::ES_CONTINUOUS);
        }
    }
}

/// Lower the calling thread's priority, mapped from a Unix nice value (0..=19):
/// 1-9 below normal, 10-18 lowest, 19 idle.
#[cfg(windows)]
pub fn set_current_thread_nice(nice: i32) -> bool {
    let priority = match nice {
        ..=0 => 0,
        1..=9 => -1,
        10..=18 => -2,
        _ => -15,
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle for this thread that needs no
    // closing; SetThreadPriority takes it and a value.
    unsafe { win::SetThreadPriority(win::GetCurrentThread(), priority) != 0 }
}

/// Let the Windows console interpret ANSI colors (Windows 10+). False if it cannot.
#[cfg(windows)]
pub fn enable_ansi_colors() -> bool {
    // SAFETY: GetStdHandle returns a borrowed handle; the mode is read into a local
    // and written back with one more flag.
    unsafe {
        let out = win::GetStdHandle(win::STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if out.is_null() || win::GetConsoleMode(out, &mut mode) == 0 {
            return false;
        }
        mode & win::ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
            || win::SetConsoleMode(out, mode | win::ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}
