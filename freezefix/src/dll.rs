//! freezefix.dll. DllMain blocks the network, then starts the patch worker. The game imports only
//! `DirectInput8Create` from dinput8.dll; it is exported here and forwarded to the system dinput8.dll, so the DLL
//! works as the game's dinput8.dll, chain-loaded by a dinput8 mod loader, or loaded by ModEngine2 (`external_dlls`),
//! which ignores the export.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::{
    Foundation::HMODULE,
    System::{
        LibraryLoader::{DisableThreadLibraryCalls, GetProcAddress, LoadLibraryW},
        SystemInformation::GetSystemDirectoryW,
        SystemServices::DLL_PROCESS_ATTACH,
    },
};

mod net;
mod patch;

const E_FAIL: i32 = 0x8000_4005_u32 as i32;

type DirectInput8CreateFn =
    unsafe extern "system" fn(*mut c_void, u32, *const c_void, *mut *mut c_void, *mut c_void) -> i32;

/// System dinput8!DirectInput8Create, resolved on the first call (outside the loader lock).
fn system_create() -> Option<DirectInput8CreateFn> {
    static REAL: OnceLock<Option<DirectInput8CreateFn>> = OnceLock::new();
    *REAL.get_or_init(|| unsafe {
        let mut dir = [0u16; 260];
        let n = GetSystemDirectoryW(dir.as_mut_ptr(), dir.len() as u32) as usize;
        if n == 0 || n >= dir.len() {
            return None;
        }
        let mut path = dir[..n].to_vec();
        path.extend("\\dinput8.dll\0".encode_utf16());
        let h = LoadLibraryW(path.as_ptr());
        if h.is_null() {
            return None;
        }
        GetProcAddress(h, c"DirectInput8Create".as_ptr().cast())
            .map(|p| core::mem::transmute::<_, DirectInput8CreateFn>(p))
    })
}

/// # Safety
/// Same contract as the system `DirectInput8Create`; the arguments are passed through unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectInput8Create(
    hinst: *mut c_void,
    version: u32,
    riid: *const c_void,
    out: *mut *mut c_void,
    outer: *mut c_void,
) -> i32 {
    match system_create() {
        Some(f) => unsafe { f(hinst, version, riid, out, outer) },
        None => E_FAIL,
    }
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe { DisableThreadLibraryCalls(module) };
        patch::open_log();
        net::block();
        patch::start();
    }
    1
}
