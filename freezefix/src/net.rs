//! Hard-wired network block: whenever this DLL is loaded, `ws2_32!WSAStartup` is overwritten with a jump to a
//! function that always fails with WSASYSNOTREADY, so Winsock is never initialized and no socket in the process
//! can be opened. There is no switch to turn it off. If the block cannot be installed, the game is terminated.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicBool, Ordering},
};

use windows_sys::Win32::System::{
    Diagnostics::Debug::{FlushInstructionCache, RtlLookupFunctionEntry},
    LibraryLoader::{GetModuleHandleW, GetProcAddress},
    Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect},
    Threading::GetCurrentProcess,
};

use crate::patch::{fail, log_line};

const WSASYSNOTREADY: i32 = 10091;

/// `mov rax, imm64; jmp rax`.
fn abs_jmp(to: usize) -> [u8; 12] {
    let mut j = [0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
    j[2..10].copy_from_slice(&(to as u64).to_le_bytes());
    j
}

extern "system" fn refuse_wsastartup(_version: u16, _data: *mut c_void) -> i32 {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log_line("network blocked: WSAStartup call refused (further calls are refused silently)");
    }
    WSASYSNOTREADY
}

/// Install the block. Called synchronously from DllMain: the game imports WS2_32.dll statically, so it is already
/// mapped, and no game code has run yet.
pub fn block() {
    let module = unsafe { GetModuleHandleW(windows_sys::w!("ws2_32.dll")) };
    if module.is_null() {
        fail("network block: ws2_32.dll is not loaded".into());
    }
    let Some(entry) = (unsafe { GetProcAddress(module, c"WSAStartup".as_ptr().cast()) }) else {
        fail("network block: WSAStartup not found".into());
    };
    let at = entry as usize;
    let jmp = abs_jmp(refuse_wsastartup as *const () as usize);
    let mut image = 0u64;
    let rf = unsafe { RtlLookupFunctionEntry(at as u64, &mut image, std::ptr::null_mut()) };
    if rf.is_null() {
        fail(format!("network block: no unwind entry for WSAStartup @{at:#x}"));
    }
    let (begin, end) =
        unsafe { (image as usize + (*rf).BeginAddress as usize, image as usize + (*rf).EndAddress as usize) };
    if begin != at || end - begin < jmp.len() {
        fail(format!("network block: unexpected WSAStartup layout [{begin:#x},{end:#x}) @{at:#x}"));
    }
    let mut old = 0;
    if unsafe { VirtualProtect(at as *const c_void, jmp.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        fail(format!("network block: VirtualProtect @{at:#x}"));
    }
    unsafe { std::ptr::copy_nonoverlapping(jmp.as_ptr(), at as *mut u8, jmp.len()) };
    let mut tmp = 0;
    unsafe {
        VirtualProtect(at as *const c_void, jmp.len(), old, &mut tmp);
        FlushInstructionCache(GetCurrentProcess(), at as *const c_void, jmp.len());
    }
    if unsafe { std::slice::from_raw_parts(at as *const u8, jmp.len()) } != jmp {
        fail(format!("network block: read-back mismatch @{at:#x}"));
    }
    log_line(&format!("network blocked: ws2_32!WSAStartup @{at:#x} always fails (WSASYSNOTREADY)"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_jmp_encoding() {
        assert_eq!(
            abs_jmp(0x1122_3344_5566_7788),
            [0x48, 0xB8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xFF, 0xE0]
        );
    }

    #[test]
    fn block_refuses_real_wsastartup() {
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
        type Startup = unsafe extern "system" fn(u16, *mut c_void) -> i32;
        let ws2 = unsafe { LoadLibraryW(windows_sys::w!("ws2_32.dll")) };
        assert!(!ws2.is_null());
        block();
        let f: Startup = unsafe { std::mem::transmute(GetProcAddress(ws2, c"WSAStartup".as_ptr().cast()).unwrap()) };
        let mut data = [0u8; 512];
        assert_eq!(unsafe { f(0x0202, data.as_mut_ptr().cast()) }, WSASYSNOTREADY);
    }

    #[test]
    fn refusal_code() {
        assert_eq!(refuse_wsastartup(0x0202, std::ptr::null_mut()), WSASYSNOTREADY);
    }
}
