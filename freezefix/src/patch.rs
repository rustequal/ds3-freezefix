//! freezefix core: inside the running game, lay down the overrides of `bodies.json` (studio bytes of one hooked
//! function entry and two NOP'd hash-loop gates), leave every other byte (the protection included) untouched, and
//! keep a watchdog that re-applies the overrides whenever the protection reverts them. Log: freezefix.log next to
//! this DLL.

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    ptr::null,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HMODULE, INVALID_HANDLE_VALUE, SYSTEMTIME},
    Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW},
    System::{
        Diagnostics::{
            Debug::{CONTEXT, CONTEXT_CONTROL_AMD64, FlushInstructionCache, GetThreadContext},
            ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next},
        },
        LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
            GetModuleHandleExW, GetModuleHandleW,
        },
        Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect},
        SystemInformation::GetLocalTime,
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
            THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME, TerminateProcess,
        },
    },
};

const BODIES: &str = include_str!("bodies.json");
const WAIT_TIMEOUT: Duration = Duration::from_secs(180);
const SETTLE: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(250);
const FAIL_EXIT_CODE: u32 = 0xF2EE_F1C5;
const MAX_THREADS: usize = 8192;

// Supported game: DarkSoulsIII.exe 1.15.2.0. VS_FIXEDFILEINFO packs a.b into MS and c.d into LS.
const WANT_MS: u32 = (1 << 16) | 15;
const WANT_LS: u32 = 2 << 16;

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub(crate) fn log_line(msg: &str) {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let line = format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}] {msg}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

macro_rules! log {
    ($($a:tt)*) => {{ log_line(&format!($($a)*)); }};
}

/// A hooked function entry: `orig` is the live window as the protection leaves it, `studio` the original compiler
/// bytes; only `[lo, hi)` differs and is written.
#[derive(Debug)]
struct Body {
    begin: usize,
    orig: Vec<u8>,
    studio: Vec<u8>,
    lo: usize,
    hi: usize,
}

/// A fixed byte override at a known RVA (a hash-loop gate): `orig` is replaced by `patched` in place.
#[derive(Debug)]
struct Raw {
    name: String,
    rva: usize,
    orig: Vec<u8>,
    patched: Vec<u8>,
}

fn hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex string {s}");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn rva(v: &serde_json::Value) -> usize {
    usize::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

fn table() -> (Vec<Body>, Vec<Raw>) {
    let doc: serde_json::Value = serde_json::from_str(BODIES).unwrap();
    let bodies = doc["bodies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| Body {
            begin: rva(&b["begin"]),
            orig: hex(b["orig"].as_str().unwrap()),
            studio: hex(b["studio"].as_str().unwrap()),
            lo: rva(&b["patch"][0]),
            hi: rva(&b["patch"][1]),
        })
        .collect();
    let raws = doc["raw_patches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| Raw {
            name: r["name"].as_str().unwrap().to_string(),
            rva: rva(&r["rva"]),
            orig: hex(r["orig"].as_str().unwrap()),
            patched: hex(r["patched"].as_str().unwrap()),
        })
        .collect();
    (bodies, raws)
}

fn at(addr: usize, n: usize) -> &'static [u8] {
    unsafe { std::slice::from_raw_parts(addr as *const u8, n) }
}

fn live(base: usize, b: &Body) -> &'static [u8] {
    at(base + b.begin, b.orig.len())
}

/// Unrecoverable: the image may be half-written, so stop the game rather than let it run in that state.
pub(crate) fn fail(msg: String) -> ! {
    log!("FAIL: {msg}; terminating the game");
    unsafe { TerminateProcess(GetCurrentProcess(), FAIL_EXIT_CODE) };
    unreachable!()
}

#[repr(C, align(16))]
struct Ctx(CONTEXT);

/// Suspends every other thread of the process; returns their handles, or the tid whose rip is inside a span.
fn suspend_others(spans: &[(usize, usize)]) -> Result<Vec<HANDLE>, u32> {
    let (pid, me) = unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        fail("CreateToolhelp32Snapshot".into());
    }
    let mut te = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..unsafe { std::mem::zeroed() } };
    let mut held = Vec::with_capacity(MAX_THREADS);
    let mut inside = None;
    let mut ok = unsafe { Thread32First(snap, &mut te) };
    while ok != 0 {
        if te.th32OwnerProcessID == pid && te.th32ThreadID != me && held.len() < MAX_THREADS {
            let h = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT, 0, te.th32ThreadID) };
            if !h.is_null() {
                if unsafe { SuspendThread(h) } == u32::MAX {
                    unsafe { CloseHandle(h) };
                } else {
                    held.push(h);
                    let mut c: Ctx = unsafe { std::mem::zeroed() };
                    c.0.ContextFlags = CONTEXT_CONTROL_AMD64;
                    if unsafe { GetThreadContext(h, &mut c.0) } == 0 {
                        fail(format!("GetThreadContext tid {}", te.th32ThreadID));
                    }
                    let rip = c.0.Rip as usize;
                    if spans.iter().any(|&(lo, hi)| lo <= rip && rip < hi) {
                        inside = Some(te.th32ThreadID);
                        break;
                    }
                }
            }
        }
        ok = unsafe { Thread32Next(snap, &mut te) };
    }
    unsafe { CloseHandle(snap) };
    match inside {
        None => Ok(held),
        Some(tid) => {
            resume(held);
            Err(tid)
        }
    }
}

fn resume(held: Vec<HANDLE>) {
    for h in held {
        unsafe {
            ResumeThread(h);
            CloseHandle(h);
        }
    }
}

fn write(at: usize, bytes: &[u8]) {
    let mut old = 0;
    if unsafe { VirtualProtect(at as *const c_void, bytes.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        fail(format!("VirtualProtect {at:#x}"));
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len()) };
    let mut tmp = 0;
    unsafe {
        VirtualProtect(at as *const c_void, bytes.len(), old, &mut tmp);
        FlushInstructionCache(GetCurrentProcess(), at as *const c_void, bytes.len());
    }
}

fn module_version() -> Option<(u32, u32)> {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let mut handle = 0u32;
    let size = unsafe { GetFileVersionInfoSizeW(buf.as_ptr(), &mut handle) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(buf.as_ptr(), 0, size, data.as_mut_ptr() as *mut c_void) } == 0 {
        return None;
    }
    let sub: Vec<u16> = "\\\0".encode_utf16().collect();
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    if unsafe { VerQueryValueW(data.as_ptr() as *const c_void, sub.as_ptr(), &mut ptr, &mut len) } == 0 || ptr.is_null()
    {
        return None;
    }
    let ffi = unsafe { &*(ptr as *const VS_FIXEDFILEINFO) };
    Some((ffi.dwFileVersionMS, ffi.dwFileVersionLS))
}

fn ver_str(v: Option<(u32, u32)>) -> String {
    match v {
        Some((ms, ls)) => format!("{}.{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff),
        None => "unknown".into(),
    }
}

/// Every body holds its hooked window and every raw site its original bytes (the protection has finished setting
/// up the image); only then are the overrides valid to lay down.
fn ready(base: usize, bodies: &[Body], raws: &[Raw]) -> bool {
    bodies.iter().all(|b| live(base, b) == b.orig) && raws.iter().all(|r| at(base + r.rva, r.orig.len()) == r.orig)
}

fn applied(base: usize, bodies: &[Body], raws: &[Raw]) -> bool {
    bodies.iter().all(|b| live(base, b) == b.studio)
        && raws.iter().all(|r| at(base + r.rva, r.patched.len()) == r.patched)
}

fn spans(base: usize, bodies: &[Body], raws: &[Raw]) -> Vec<(usize, usize)> {
    let mut s: Vec<(usize, usize)> = bodies.iter().map(|b| (base + b.lo, base + b.hi)).collect();
    s.extend(raws.iter().map(|r| (base + r.rva, base + r.rva + r.orig.len())));
    s
}

/// Suspend the other threads (none may be executing inside a span), write every override, resume, verify.
fn apply(base: usize, bodies: &[Body], raws: &[Raw], spans: &[(usize, usize)]) {
    let held = loop {
        match suspend_others(spans) {
            Ok(h) => break h,
            Err(tid) => {
                log!("thread {tid} is inside a patch span, retrying");
                thread::sleep(Duration::from_millis(1));
            }
        }
    };
    for b in bodies {
        write(base + b.lo, &b.studio[b.lo - b.begin..b.hi - b.begin]);
    }
    for r in raws {
        write(base + r.rva, &r.patched);
    }
    resume(held);
    for b in bodies {
        if live(base, b) != b.studio {
            fail(format!("body {:#x} read-back != studio", b.begin));
        }
    }
    for r in raws {
        if at(base + r.rva, r.patched.len()) != r.patched {
            fail(format!("raw {} @{:#x} read-back != patched", r.name, r.rva));
        }
    }
}

fn run() {
    let base = unsafe { GetModuleHandleW(null()) } as usize;
    let v = module_version();
    if v != Some((WANT_MS, WANT_LS)) {
        log!("DarkSoulsIII.exe version {} != 1.15.2.0; not patching", ver_str(v));
        return;
    }
    let (bodies, raws) = table();
    log!("freezefix: base {base:#x}, version 1.15.2.0, {} inplace + {} raw patches", bodies.len(), raws.len());
    // Loaded as a static import or by a mod loader we run very early; let the SteamStub unpacker and the loader
    // finish mapping the real image before the first read.
    thread::sleep(SETTLE);
    let start = Instant::now();
    while !ready(base, &bodies, &raws) {
        if start.elapsed() > WAIT_TIMEOUT {
            log!(
                "patch sites never held their expected bytes within {} s (another mod?); not patching",
                WAIT_TIMEOUT.as_secs()
            );
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    log!("all patch sites in place after {:.1} ms", start.elapsed().as_secs_f64() * 1e3);
    let spans = spans(base, &bodies, &raws);
    apply(base, &bodies, &raws, &spans);
    for b in &bodies {
        log!("patched inplace {:#x} [{:#x},{:#x}) -> original bytes", b.begin, b.lo, b.hi);
    }
    for r in &raws {
        log!("patched raw {} @{:#x} {:02x?} -> {:02x?}", r.name, base + r.rva, r.orig, r.patched);
    }
    // Watchdog: re-apply on every revert. Only the first revert is logged (as proof the watchdog engages), so
    // the log stays small across a long play session.
    let last = Instant::now();
    let mut logged = false;
    loop {
        thread::sleep(POLL);
        if applied(base, &bodies, &raws) {
            continue;
        }
        if !logged {
            for b in bodies.iter().filter(|b| live(base, b) != b.studio) {
                log!("inplace {:#x} REVERTED: first5 {:02x?}", b.begin, &live(base, b)[..5.min(b.orig.len())]);
            }
            for r in raws.iter().filter(|r| at(base + r.rva, r.patched.len()) != r.patched) {
                log!("raw {} @{:#x} REVERTED: {:02x?}", r.name, base + r.rva, at(base + r.rva, r.patched.len()));
            }
        }
        apply(base, &bodies, &raws, &spans);
        if !logged {
            log!("re-applied after {:.3}s; further reverts are handled silently", last.elapsed().as_secs_f64());
            logged = true;
        }
    }
}

fn log_path() -> PathBuf {
    let mut module: HMODULE = std::ptr::null_mut();
    let mut buf = [0u16; 1024];
    let n = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            log_path as *const () as *const u16,
            &mut module,
        );
        GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32)
    } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).with_file_name("freezefix.log")
}

/// Open freezefix.log next to this DLL. Safe to call from DllMain.
pub fn open_log() {
    *LOG.lock().unwrap() = OpenOptions::new().create(true).append(true).open(log_path()).ok();
}

/// Start the patch + watchdog worker thread. Safe to call from DllMain.
pub fn start() {
    thread::spawn(run);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        let (bodies, raws) = table();
        assert!(!bodies.is_empty() && !raws.is_empty());
        for b in &bodies {
            assert_eq!(b.orig.len(), b.studio.len(), "body {:#x}", b.begin);
            assert!(b.begin <= b.lo && b.lo < b.hi && b.hi <= b.begin + b.orig.len(), "body {:#x}", b.begin);
            let (l, h) = (b.lo - b.begin, b.hi - b.begin);
            assert_eq!(b.orig[..l], b.studio[..l]);
            assert_eq!(b.orig[h..], b.studio[h..]);
            assert_ne!(b.orig[l..h], b.studio[l..h]);
        }
        for r in &raws {
            assert_eq!(r.orig.len(), r.patched.len(), "raw {}", r.name);
            assert_ne!(r.orig, r.patched, "raw {}", r.name);
            assert!(r.patched.iter().all(|&x| x == 0x90), "raw {} is not a NOP fill", r.name);
        }
    }

    #[test]
    fn spans_cover_exactly_the_written_bytes() {
        let (bodies, raws) = table();
        let s = spans(0x1000, &bodies, &raws);
        assert_eq!(s.len(), bodies.len() + raws.len());
        assert!(s.iter().all(|&(lo, hi)| lo < hi));
        assert_eq!(s[0], (0x1000 + bodies[0].lo, 0x1000 + bodies[0].hi));
    }

    #[test]
    fn version_pack_is_1_15_2_0() {
        assert_eq!(ver_str(Some((WANT_MS, WANT_LS))), "1.15.2.0");
        assert_eq!(ver_str(None), "unknown");
    }

    #[test]
    fn hex_and_rva_parse() {
        assert_eq!(hex("e9ff00"), [0xe9, 0xff, 0x00]);
        assert_eq!(rva(&serde_json::json!("0x8f55f0")), 0x8f55f0);
    }
}
