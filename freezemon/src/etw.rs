use std::{
    mem::{size_of, size_of_val},
    ptr,
};

use windows_sys::{
    Win32::{
        Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, GetLastError, HANDLE, LUID},
        Security::{
            AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
            TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        System::{
            Diagnostics::Etw::*,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    },
    core::GUID,
};

pub const KERNEL: &str = "NT Kernel Logger";
pub const USER: &str = "freezemon-dxgi";
pub const DXGI: GUID = GUID::from_u128(0xca11c036_0102_4a2d_a6ad_f03cfed5d3c9);
pub const PERFINFO: GUID = GUID::from_u128(0xce1dbfb4_137e_4da6_87b0_3f59aa102cbc);
pub const IMAGE: GUID = GUID::from_u128(0x2cb15d1d_5fc1_11d2_abe1_00a0c911f518);
pub const THREAD: GUID = GUID::from_u128(0x3d6fa8d1_fe05_11d0_9dda_00c04fd7ba7c);
pub const STACKWALK: GUID = GUID::from_u128(0xdef2fe46_7bd6_4b80_bd94_f57fe20d0ce3);
pub const PROCESS: GUID = GUID::from_u128(0x3d6fa8d0_fe05_11d0_9dda_00c04fd7ba7c);

pub type Result<T> = std::result::Result<T, String>;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn check(code: u32, what: &str) -> Result<()> {
    if code == ERROR_SUCCESS { Ok(()) } else { Err(format!("{what}: win32 error {code}")) }
}

pub struct Props(Vec<u64>);

impl Props {
    fn new() -> Self {
        let n = size_of::<EVENT_TRACE_PROPERTIES>() + 2048;
        let mut v = vec![0u64; n.div_ceil(8)];
        let p = v.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            (*p).Wnode.BufferSize = n as u32;
            (*p).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        }
        Self(v)
    }

    pub fn get(&mut self) -> &mut EVENT_TRACE_PROPERTIES {
        unsafe { &mut *self.0.as_mut_ptr().cast() }
    }
}

/// Stops a session by name; returns its final properties (lost-event counters) if it was running.
pub fn stop(name: &str) -> Option<Props> {
    let mut p = Props::new();
    let w = wide(name);
    let rc = unsafe { ControlTraceW(CONTROLTRACE_HANDLE { Value: 0 }, w.as_ptr(), p.get(), EVENT_TRACE_CONTROL_STOP) };
    (rc == ERROR_SUCCESS).then_some(p)
}

fn enable_privilege(name: &str) -> Result<()> {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return Err(format!("OpenProcessToken: {}", GetLastError()));
        }
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        let w = wide(name);
        if LookupPrivilegeValueW(ptr::null(), w.as_ptr(), &mut luid) == 0 {
            CloseHandle(token);
            return Err(format!("LookupPrivilegeValueW({name}): {}", GetLastError()));
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let ok = AdjustTokenPrivileges(token, 0, &tp, 0, ptr::null_mut(), ptr::null_mut());
        let err = GetLastError();
        CloseHandle(token);
        if ok == 0 || err == ERROR_NOT_ALL_ASSIGNED {
            return Err(format!("{name} not held: run freezemon as administrator"));
        }
    }
    Ok(())
}

fn start(name: &str, setup: impl FnOnce(&mut EVENT_TRACE_PROPERTIES)) -> Result<CONTROLTRACE_HANDLE> {
    stop(name);
    let mut p = Props::new();
    let props = p.get();
    props.Wnode.ClientContext = 1;
    props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    props.BufferSize = 256;
    props.MinimumBuffers = 64;
    props.MaximumBuffers = 1024;
    props.FlushTimer = 1;
    setup(props);
    let mut h = CONTROLTRACE_HANDLE { Value: 0 };
    let w = wide(name);
    check(unsafe { StartTraceW(&mut h, w.as_ptr(), p.get()) }, &format!("StartTraceW({name})"))?;
    Ok(h)
}

/// Kernel session: sampled profile + image/thread/process events. `interval` is in 100 ns units;
/// returns the interval the system actually applied.
pub fn start_kernel(interval: u32) -> Result<u32> {
    enable_privilege("SeSystemProfilePrivilege")?;
    let iv = TRACE_PROFILE_INTERVAL { Source: 0, Interval: interval };
    check(
        unsafe {
            TraceSetInformation(
                CONTROLTRACE_HANDLE { Value: 0 },
                TraceSampledProfileIntervalInfo,
                (&iv as *const TRACE_PROFILE_INTERVAL).cast(),
                size_of::<TRACE_PROFILE_INTERVAL>() as u32,
            )
        },
        "TraceSetInformation(profile interval)",
    )?;
    let h = start(KERNEL, |p| {
        p.Wnode.Guid = SystemTraceControlGuid;
        p.EnableFlags =
            EVENT_TRACE_FLAG_PROFILE | EVENT_TRACE_FLAG_IMAGE_LOAD | EVENT_TRACE_FLAG_THREAD | EVENT_TRACE_FLAG_PROCESS;
    })?;
    let ids = [CLASSIC_EVENT_ID { EventGuid: PERFINFO, Type: 46, Reserved: [0; 7] }];
    check(
        unsafe { TraceSetInformation(h, TraceStackTracingInfo, ids.as_ptr().cast(), size_of_val(&ids) as u32) },
        "TraceSetInformation(stack tracing)",
    )?;
    let mut got = TRACE_PROFILE_INTERVAL { Source: 0, Interval: 0 };
    let mut len = 0u32;
    check(
        unsafe {
            TraceQueryInformation(
                CONTROLTRACE_HANDLE { Value: 0 },
                TraceSampledProfileIntervalInfo,
                (&mut got as *mut TRACE_PROFILE_INTERVAL).cast(),
                size_of::<TRACE_PROFILE_INTERVAL>() as u32,
                &mut len,
            )
        },
        "TraceQueryInformation(profile interval)",
    )?;
    if got.Interval == 0 {
        return Err("profile interval reported as 0".into());
    }
    Ok(got.Interval)
}

/// User session with the DXGI provider (Present events).
pub fn start_user() -> Result<()> {
    let h = start(USER, |_| {})?;
    check(
        unsafe {
            EnableTraceEx2(
                h,
                &DXGI,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                TRACE_LEVEL_INFORMATION as u8,
                u64::MAX,
                0,
                0,
                ptr::null(),
            )
        },
        "EnableTraceEx2(DXGI)",
    )
}

/// Blocks delivering real-time events of session `name` to `cb` until the session stops.
pub fn consume(name: &str, cb: PEVENT_RECORD_CALLBACK) -> Result<()> {
    let mut w = wide(name);
    let mut lf: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    lf.LoggerName = w.as_mut_ptr();
    lf.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
    lf.Anonymous2.EventRecordCallback = cb;
    let h = unsafe { OpenTraceW(&mut lf) };
    if h.Value == u64::MAX {
        return Err(format!("OpenTraceW({name}): {}", unsafe { GetLastError() }));
    }
    let rc = unsafe { ProcessTrace(&h, 1, ptr::null(), ptr::null()) };
    unsafe { CloseTrace(h) };
    check(rc, &format!("ProcessTrace({name})"))
}

pub fn lost(p: &mut Props) -> (u32, u32) {
    let p = p.get();
    (p.EventsLost, p.RealTimeBuffersLost)
}
