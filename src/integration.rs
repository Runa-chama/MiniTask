//! OS 連携: タスクマネージャー置換 (IFEO) / シェルメニュー / ウィンドウ操作。

use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsIconic, SetForegroundWindow, ShowWindow,
    SW_RESTORE, SW_SHOW,
};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

type Handle = *mut std::ffi::c_void;

const IFEO_PATH: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe";

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
// デスクトップ/フォルダ背景のシェルメニュー
// ---------------------------------------------------------------------------

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
// ウィンドウ操作
// ---------------------------------------------------------------------------

unsafe extern "system" fn enum_focus_proc(hwnd: Handle, _lparam: isize) -> i32 {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len > 0 {
            let mut title = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            let text = String::from_utf16_lossy(&title);
            if text.contains("MiniTask") && text.contains("軽量タスクマネージャー") {
                ShowWindow(hwnd, SW_SHOW);
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
                return 0;
            }
        }
        1
    }
}

/// "MiniTask" を含むウィンドウを前面化する。
pub fn focus_window() {
    unsafe {
        EnumWindows(Some(enum_focus_proc), 0);
    }
}

/// 既存の MiniTask インスタンスを前面化できたら true。
pub fn focus_existing_instance() -> bool {
    // EnumWindows は見つかったら 0 を返す（コールバックが 0 を返した）。
    // Windows API の EnumWindows 自体の戻り値: コールバックが途中で 0 を返すと 0。
    unsafe extern "system" fn proc(hwnd: Handle, found: isize) -> i32 {
        unsafe {
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return 1;
            }
            let mut title = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            let text = String::from_utf16_lossy(&title);
            if text.contains("MiniTask") && text.contains("軽量タスクマネージャー") {
                ShowWindow(hwnd, SW_SHOW);
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
                *(found as *mut bool) = true;
                return 0;
            }
            1
        }
    }
    let mut found = false;
    unsafe {
        EnumWindows(Some(proc), &mut found as *mut bool as isize);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifeo_constants() {
        assert!(IFEO_PATH.contains("Image File Execution Options"));
    }
}
