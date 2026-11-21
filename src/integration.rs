//! OS 連携: タスクマネージャー置換 (IFEO) / explorer 注入 / タスクバー監視。

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, INVALID_HANDLE_VALUE, POINT,
};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, OpenProcess, WaitForSingleObject, LPTHREAD_START_ROUTINE,
    PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_WRITE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowW, GetAncestor, GetClassNameW, GetWindowThreadProcessId,
    SetForegroundWindow, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, WindowFromPoint,
    GetWindowTextLengthW, GetWindowTextW, EnumWindows, GA_ROOT, HC_ACTION, SW_RESTORE, WH_MOUSE_LL,
    WM_RBUTTONUP, MSLLHOOKSTRUCT,
};
use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY,
};
use winreg::RegKey;

type Handle = *mut c_void;

const IFEO_PATH: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe";
const HOOK_DLL: &str = "minitask_hook.dll";
const STATUS_PATH: &str = r"Software\MiniTask";

// ---------------------------------------------------------------------------
// IFEO (Ctrl+Shift+Esc 置換)
// ---------------------------------------------------------------------------

pub fn set_replace_taskmgr(enable: bool, exe_path: &str) -> Result<(), String> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if enable {
        let (key, _) = hklm
            .create_subkey_with_flags(IFEO_PATH, KEY_SET_VALUE | KEY_WOW64_64KEY)
            .map_err(|e| format!("IFEO キーを開けません (管理者権限が必要): {e}"))?;
        key.set_value("Debugger", &format!("\"{exe_path}\""))
            .map_err(|e| format!("Debugger 値を設定できません: {e}"))?;
    } else {
        let key = hklm
            .open_subkey_with_flags(IFEO_PATH, KEY_SET_VALUE | KEY_WOW64_64KEY)
            .map_err(|e| format!("IFEO キーを開けません: {e}"))?;
        key.delete_value("Debugger")
            .map_err(|e| format!("Debugger 値を削除できません: {e}"))?;
    }
    Ok(())
}

pub fn debugger_value() -> Option<String> {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(IFEO_PATH, KEY_READ | KEY_WOW64_64KEY)
        .ok()?;
    key.get_value::<String, _>("Debugger").ok()
}

pub fn is_replace_active() -> bool {
    let Some(v) = debugger_value() else {
        return false;
    };
    let Some(exe) = std::env::current_exe().ok() else {
        return false;
    };
    v.trim_matches('"')
        .eq_ignore_ascii_case(&exe.to_string_lossy())
}

pub fn pause_ifeo_for_taskmgr() -> Result<Option<String>, String> {
    let old = debugger_value();
    if old.is_some() {
        set_replace_taskmgr(false, "")?;
    }
    let system32 = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let path = format!("{system32}\\System32\\taskmgr.exe");
    std::process::Command::new(&path)
        .spawn()
        .map_err(|e| format!("タスクマネージャーを起動できません: {e}"))?;
    Ok(old)
}

pub fn restore_ifeo(old: Option<String>) {
    if let Some(v) = old {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok((key, _)) =
            hklm.create_subkey_with_flags(IFEO_PATH, KEY_SET_VALUE | KEY_WOW64_64KEY)
        {
            let _ = key.set_value("Debugger", &v);
        }
    }
}

// ---------------------------------------------------------------------------
// explorer 注入
// ---------------------------------------------------------------------------

pub fn hook_dll_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(HOOK_DLL))
}

fn explorer_pid() -> Option<u32> {
    unsafe {
        let class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (pid != 0).then_some(pid)
    }
}

pub fn inject_hook() -> Result<(), String> {
    let dll = hook_dll_path().ok_or("running exe path unknown")?;
    if !dll.exists() {
        return Err(format!("{} が見つかりません", dll.display()));
    }
    let pid = explorer_pid().ok_or("explorer.exe not found")?;
    unsafe {
        let process = OpenProcess(
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE,
            0,
            pid,
        );
        if process.is_null() || process == INVALID_HANDLE_VALUE {
            return Err(format!("explorer を開けません (error {})", GetLastError()));
        }
        let path: Vec<u16> = dll
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let size = path.len() * 2;
        let memory = VirtualAllocEx(process, std::ptr::null(), size, MEM_COMMIT, PAGE_READWRITE);
        if memory.is_null() {
            let _ = CloseHandle(process);
            return Err("explorer にメモリを確保できません".to_owned());
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(process, memory, path.as_ptr().cast(), size, &mut written);
        if ok == 0 {
            let _ = VirtualFreeEx(process, memory, 0, MEM_RELEASE);
            let _ = CloseHandle(process);
            return Err("DLL パスを書き込めません".to_owned());
        }
        let kernel32 = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(
            "kernel32.dll\0".encode_utf16().collect::<Vec<u16>>().as_ptr(),
        );
        let load_library = GetProcAddress(kernel32, b"LoadLibraryW\0".as_ptr());
        let start: LPTHREAD_START_ROUTINE = std::mem::transmute(load_library);
        let thread = CreateRemoteThread(
            process,
            std::ptr::null(),
            0,
            start,
            memory.cast(),
            0,
            std::ptr::null_mut(),
        );
        let result = if thread.is_null() {
            Err(format!("リモートスレッドを作成できません (error {})", GetLastError()))
        } else {
            let _ = WaitForSingleObject(thread, 5000);
            let _ = CloseHandle(thread);
            Ok(())
        };
        let _ = VirtualFreeEx(process, memory, 0, MEM_RELEASE);
        let _ = CloseHandle(process);
        result
    }
}

pub fn uninject_hook() -> Result<(), String> {
    let Some(pid) = explorer_pid() else {
        return Ok(());
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid);
        if snapshot == INVALID_HANDLE_VALUE {
            return Ok(());
        }
        let mut entry: MODULEENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut base: Handle = std::ptr::null_mut();
        if Module32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let name = String::from_utf16_lossy(&entry.szModule)
                    .trim_end_matches('\0')
                    .to_lowercase();
                if name == HOOK_DLL {
                    base = entry.modBaseAddr as *mut c_void;
                    break;
                }
                if Module32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        if base.is_null() {
            return Ok(());
        }
        let process = OpenProcess(
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION,
            0,
            pid,
        );
        if process.is_null() {
            return Err("explorer を開けません".to_owned());
        }
        let kernel32 = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(
            "kernel32.dll\0".encode_utf16().collect::<Vec<u16>>().as_ptr(),
        );
        let free_library = GetProcAddress(kernel32, b"FreeLibrary\0".as_ptr());
        let start: LPTHREAD_START_ROUTINE = std::mem::transmute(free_library);
        let thread = CreateRemoteThread(
            process,
            std::ptr::null(),
            0,
            start,
            base as *const c_void,
            0,
            std::ptr::null_mut(),
        );
        if thread.is_null() {
            let _ = CloseHandle(process);
            return Err("リモートスレッドを作成できません".to_owned());
        }
        let _ = WaitForSingleObject(thread, 3000);
        let _ = CloseHandle(thread);
        let _ = CloseHandle(process);
        Ok(())
    }
}

pub fn hook_status() -> (bool, bool) {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(STATUS_PATH, KEY_READ)
    else {
        return (false, false);
    };
    let installed = key
        .get_value::<u32, _>("HookInstalled")
        .map(|v| v != 0)
        .unwrap_or(false);
    let fired = key
        .get_value::<u32, _>("HookFired")
        .map(|v| v != 0)
        .unwrap_or(false);
    (installed, fired)
}

pub fn set_shell_menu(enable: bool, exe_path: &str) -> Result<(), String> {
    let paths = [
        r"Software\Classes\DesktopBackground\Shell\MiniTask\command",
        r"Software\Classes\Directory\Background\shell\MiniTask\command",
    ];
    for path in paths {
        if enable {
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(path)
                .map_err(|e| e.to_string())?;
            key.set_value("", &format!("\"{exe_path}\""))
                .map_err(|e| e.to_string())?;
            let parent = RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey(path.rsplit_once('\\').unwrap().0)
                .map_err(|e| e.to_string())?;
            let _ = parent.set_value("MUIVerb", &"MiniTask".to_owned());
        } else {
            let _ = RegKey::predef(HKEY_CURRENT_USER)
                .delete_subkey_all(path.rsplit_once('\\').unwrap().0);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// タスクバー右クリック監視
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct TrayClick {
    pub x: i32,
    pub y: i32,
}

static TRAY_SENDER: OnceLock<Mutex<Option<Sender<TrayClick>>>> = OnceLock::new();

fn sender() -> &'static Mutex<Option<Sender<TrayClick>>> {
    TRAY_SENDER.get_or_init(|| Mutex::new(None))
}

unsafe fn is_tray_window(hwnd: Handle) -> bool { unsafe {
    if hwnd.is_null() {
        return false;
    }
    let root = GetAncestor(hwnd, GA_ROOT);
    let mut class = [0u16; 128];
    let n = GetClassNameW(root, class.as_mut_ptr(), class.len() as i32);
    if n == 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&class[..n as usize]).to_lowercase();
    name == "shell_traywnd" || name == "secondarytraywnd"
}}

unsafe extern "system" fn mouse_hook(code: i32, wparam: usize, lparam: isize) -> isize { unsafe {
    if code == HC_ACTION as i32 && wparam == WM_RBUTTONUP as usize {
        let data = &*(lparam as *const MSLLHOOKSTRUCT);
        let hwnd = WindowFromPoint(POINT {
            x: data.pt.x,
            y: data.pt.y,
        });
        if is_tray_window(hwnd) {
            if let Ok(guard) = sender().lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(TrayClick {
                        x: data.pt.x,
                        y: data.pt.y,
                    });
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}}

pub fn start_tray_monitor() -> (Receiver<TrayClick>, Option<Handle>) {
    let (tx, rx) = mpsc::channel();
    if let Ok(mut guard) = sender().lock() {
        *guard = Some(tx);
    }
    let hook = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook),
            std::ptr::null_mut(),
            0,
        )
    };
    (rx, (!hook.is_null()).then_some(hook))
}

pub fn stop_tray_monitor(hook: Option<Handle>) {
    if let Some(h) = hook {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
    if let Ok(mut guard) = sender().lock() {
        *guard = None;
    }
}

unsafe extern "system" fn enum_proc(hwnd: Handle, _lparam: isize) -> i32 { unsafe {
    let len = GetWindowTextLengthW(hwnd);
    if len > 0 {
        let mut title = vec![0u16; (len + 1) as usize];
        GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        if String::from_utf16_lossy(&title).contains("MiniTask") {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
            return 0;
        }
    }
    1
}}

pub fn focus_window() {
    unsafe {
        EnumWindows(Some(enum_proc), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifeo_constants() {
        assert!(IFEO_PATH.contains("Image File Execution Options"));
        assert_eq!(HOOK_DLL, "minitask_hook.dll");
    }
}
