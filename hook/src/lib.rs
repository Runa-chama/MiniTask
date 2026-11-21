//! explorer.exe に注入され、タスクバーの右クリックメニューへ
//! 「MiniTask」項目を追加する小さな DLL (Win10 / クラシックタスクバー用)。

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::System::Diagnostics::Debug::{
    IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS64,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_READWRITE};
use windows_sys::Win32::System::SystemServices::{IMAGE_DOS_HEADER, IMAGE_IMPORT_DESCRIPTOR};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateThread, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallWindowProcW, GetClassNameW, SetWindowLongPtrW, TPMPARAMS, WM_COMMAND,
    GWLP_WNDPROC, MF_SEPARATOR, MF_STRING,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

type Handle = *mut c_void;

const STATUS_PATH: &str = r"Software\MiniTask";
const MENU_ID: usize = 0x4D54_0001;
const MENU_TEXT: &str = "MiniTask\0";

static ORIGINAL_POPUP_EX: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_POPUP: AtomicUsize = AtomicUsize::new(0);
static IAT_EX: AtomicUsize = AtomicUsize::new(0);
static IAT_POPUP: AtomicUsize = AtomicUsize::new(0);
static OLD_PROTECT_EX: AtomicU32 = AtomicU32::new(0);
static OLD_PROTECT_POPUP: AtomicU32 = AtomicU32::new(0);
static TRAY_HWND: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_WNDPROC: AtomicUsize = AtomicUsize::new(0);
static EXE_PATH: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();

#[repr(C)]
struct ImportByName {
    hint: u16,
    name: [u8; 1],
}

fn set_status(name: &str, value: bool) {
    if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey_with_flags(STATUS_PATH, KEY_SET_VALUE)
    {
        let _ = key.set_value(name, &(value as u32));
    }
}

fn read_exe_path() -> Vec<u16> {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(STATUS_PATH, KEY_READ)
    else {
        return Vec::new();
    };
    let Ok(path) = key.get_value::<String, _>("ExePath") else {
        return Vec::new();
    };
    path.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn as_handle(v: usize) -> Handle {
    std::ptr::with_exposed_provenance_mut::<c_void>(v)
}

unsafe fn add_menu_item(hmenu: Handle, hwnd: Handle) { unsafe {
    if TRAY_HWND.load(Ordering::Relaxed) != hwnd as usize {
        if let Some(orig) = subclass_tray(hwnd) {
            TRAY_HWND.store(hwnd as usize, Ordering::Relaxed);
            ORIGINAL_WNDPROC.store(orig, Ordering::Relaxed);
        }
    }
    AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(hmenu, MF_STRING, MENU_ID, MENU_TEXT.as_ptr().cast());
    set_status("HookFired", true);
}}

unsafe fn is_tray(hwnd: Handle) -> bool { unsafe {
    if hwnd.is_null() {
        return false;
    }
    let mut class = [0u16; 128];
    let n = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if n == 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&class[..n as usize]).to_lowercase();
    name == "shell_traywnd" || name == "secondarytraywnd"
}}

unsafe extern "system" fn tray_wndproc(
    hwnd: Handle,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize { unsafe {
    if msg == WM_COMMAND && (wparam & 0xFFFF) == MENU_ID {
        launch_app();
        return 0;
    }
    let orig = ORIGINAL_WNDPROC.load(Ordering::Relaxed);
    if orig != 0 {
        let proc: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC =
            std::mem::transmute(orig);
        CallWindowProcW(proc, hwnd, msg, wparam, lparam)
    } else {
        0
    }
}}

unsafe fn subclass_tray(hwnd: Handle) -> Option<usize> { unsafe {
    let orig = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, tray_wndproc as *const () as isize);
    if orig == 0 {
        None
    } else {
        Some(orig as usize)
    }
}}

unsafe fn launch_app() { unsafe {
    let path = EXE_PATH.get_or_init(read_exe_path);
    if path.is_empty() {
        return;
    }
    let mut mutable = path.clone();
    let mut si: STARTUPINFOW = std::mem::zeroed();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    let _ = CreateProcessW(
        mutable.as_mut_ptr(),
        std::ptr::null_mut(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        0,
        std::ptr::null(),
        std::ptr::null(),
        &si,
        &mut pi,
    );
    if !pi.hProcess.is_null() {
        let _ = windows_sys::Win32::Foundation::CloseHandle(pi.hThread);
        let _ = windows_sys::Win32::Foundation::CloseHandle(pi.hProcess);
    }
}}

type PopupExFn =
    unsafe extern "system" fn(Handle, u32, i32, i32, Handle, *const TPMPARAMS) -> i32;
type PopupFn = unsafe extern "system" fn(Handle, u32, i32, i32, i32, Handle, *const RECT) -> i32;

unsafe extern "system" fn hooked_popup_ex(
    hmenu: Handle,
    flags: u32,
    x: i32,
    y: i32,
    hwnd: Handle,
    params: *const TPMPARAMS,
) -> i32 {
    unsafe {
        if is_tray(hwnd) {
            add_menu_item(hmenu, hwnd);
        }
        let orig = ORIGINAL_POPUP_EX.load(Ordering::Relaxed);
        if orig == 0 {
            return 0;
        }
        let f: PopupExFn = std::mem::transmute(orig);
        f(hmenu, flags, x, y, hwnd, params)
    }
}

unsafe extern "system" fn hooked_popup(
    hmenu: Handle,
    flags: u32,
    x: i32,
    y: i32,
    reserved: i32,
    hwnd: Handle,
    rect: *const RECT,
) -> i32 {
    unsafe {
        if is_tray(hwnd) {
            add_menu_item(hmenu, hwnd);
        }
        let orig = ORIGINAL_POPUP.load(Ordering::Relaxed);
        if orig == 0 {
            return 0;
        }
        let f: PopupFn = std::mem::transmute(orig);
        f(hmenu, flags, x, y, reserved, hwnd, rect)
    }
}

unsafe fn patch_iat(name: &str, new_fn: usize) -> Option<(usize, usize, u32)> {
    unsafe {
        let base = GetModuleHandleW(std::ptr::null()) as usize;
        if base == 0 {
            return None;
        }
        let dos = &*(base as *const IMAGE_DOS_HEADER);
        let nt = &*((base + dos.e_lfanew as usize) as *const IMAGE_NT_HEADERS64);
        let dir = &nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize];
        if dir.VirtualAddress == 0 {
            return None;
        }
        let mut desc = (base + dir.VirtualAddress as usize) as *const IMAGE_IMPORT_DESCRIPTOR;
        loop {
            if (*desc).Name == 0 && (*desc).FirstThunk == 0 {
                break;
            }
            let dll = std::ffi::CStr::from_ptr((base + (*desc).Name as usize) as *const i8);
            if dll.to_bytes().eq_ignore_ascii_case(b"user32.dll") {
                let mut thunk = (base + (*desc).FirstThunk as usize) as *mut usize;
                while *thunk != 0 {
                    let entry = *thunk;
                    if entry & (1usize << 63) == 0 {
                        let import = &*((base + entry) as *const ImportByName);
                        let fname = std::ffi::CStr::from_ptr(import.name.as_ptr() as *const i8);
                        if fname.to_bytes() == name.as_bytes() {
                            let mut old = 0u32;
                            if VirtualProtect(
                                thunk.cast(),
                                std::mem::size_of::<usize>(),
                                PAGE_READWRITE,
                                &mut old,
                            ) == 0
                            {
                                return None;
                            }
                            let previous = *thunk;
                            *thunk = new_fn;
                            let mut dummy = 0u32;
                            let _ = VirtualProtect(
                                thunk.cast(),
                                std::mem::size_of::<usize>(),
                                old,
                                &mut dummy,
                            );
                            return Some((thunk as usize, previous, old));
                        }
                    }
                    thunk = thunk.add(1);
                }
            }
            desc = desc.add(1);
        }
        None
    }
}

unsafe extern "system" fn worker(_param: *mut c_void) -> u32 {
    unsafe {
        if let Some((addr, prev, old)) = patch_iat("TrackPopupMenuEx", hooked_popup_ex as *const () as usize) {
            IAT_EX.store(addr, Ordering::Relaxed);
            ORIGINAL_POPUP_EX.store(prev, Ordering::Relaxed);
            OLD_PROTECT_EX.store(old, Ordering::Relaxed);
        }
        if let Some((addr, prev, old)) = patch_iat("TrackPopupMenu", hooked_popup as *const () as usize) {
            IAT_POPUP.store(addr, Ordering::Relaxed);
            ORIGINAL_POPUP.store(prev, Ordering::Relaxed);
            OLD_PROTECT_POPUP.store(old, Ordering::Relaxed);
        }
        set_status("HookInstalled", true);
    }
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    _hinst: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    const DLL_PROCESS_DETACH: u32 = 0;
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            let mut tid = 0u32;
            let _ = CreateThread(
                std::ptr::null(),
                0,
                Some(worker),
                std::ptr::null(),
                0,
                &mut tid,
            );
        }
    } else if reason == DLL_PROCESS_DETACH {
        unsafe {
            let ex = IAT_EX.load(Ordering::Relaxed);
            let prev = ORIGINAL_POPUP_EX.load(Ordering::Relaxed);
            if ex != 0 && prev != 0 {
                let mut dummy = 0u32;
                let _ = VirtualProtect(
                    ex as *const c_void,
                    std::mem::size_of::<usize>(),
                    PAGE_READWRITE,
                    &mut dummy,
                );
                *(ex as *mut usize) = prev;
                let _ = VirtualProtect(
                    ex as *const c_void,
                    std::mem::size_of::<usize>(),
                    OLD_PROTECT_EX.load(Ordering::Relaxed),
                    &mut dummy,
                );
            }
            let p = IAT_POPUP.load(Ordering::Relaxed);
            let prev = ORIGINAL_POPUP.load(Ordering::Relaxed);
            if p != 0 && prev != 0 {
                let mut dummy = 0u32;
                let _ = VirtualProtect(
                    p as *const c_void,
                    std::mem::size_of::<usize>(),
                    PAGE_READWRITE,
                    &mut dummy,
                );
                *(p as *mut usize) = prev;
                let _ = VirtualProtect(
                    p as *const c_void,
                    std::mem::size_of::<usize>(),
                    OLD_PROTECT_POPUP.load(Ordering::Relaxed),
                    &mut dummy,
                );
            }
            let hwnd = TRAY_HWND.load(Ordering::Relaxed);
            let orig = ORIGINAL_WNDPROC.load(Ordering::Relaxed);
            if hwnd != 0 && orig != 0 {
                let _ = SetWindowLongPtrW(as_handle(hwnd), GWLP_WNDPROC, orig as isize);
            }
        }
        set_status("HookInstalled", false);
        set_status("HookFired", false);
    }
    1
}
