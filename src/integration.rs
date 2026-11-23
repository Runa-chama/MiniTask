//! OS 連携: 起動置換 (IFEO) / シェルメニュー / ウィンドウ操作。

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, OpenProcessToken,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, EnumWindows, FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO,
    FindWindowW, FlashWindowEx, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowLongPtrW, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IsIconic,
    IsWindowVisible, LWA_ALPHA, PostMessageW, SetLayeredWindowAttributes, SetWindowLongPtrW,
    SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    GWL_EXSTYLE, WS_EX_LAYERED,
    SetForegroundWindow, SetWindowPos, ShowWindow, SwitchToThisWindow,
};
use winreg::RegKey;
use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY,
};

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

fn focus_log(msg: &str) {
    if std::env::var_os("MINITASK_DEBUG_FOCUS").is_none() {
        return;
    }
    use std::io::Write;
    let path = std::env::temp_dir().join("minitask_focus.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// 非表示・最小化・フォアグラウンドロックを考慮してウィンドウを前面化する。
unsafe fn bring_to_front(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }

        // Z-order を最上位へ
        let z = SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        SwitchToThisWindow(hwnd, 1);
        let fg1 = SetForegroundWindow(hwnd);

        // フォアグラウンドロック対策: 前面ウィンドウのスレッドに一時的に入力アタッチ
        let fg = GetForegroundWindow();
        let mut fg_thread = 0u32;
        if !fg.is_null() {
            fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        }
        let cur_thread = GetCurrentThreadId();
        let attached = fg_thread != 0
            && fg_thread != cur_thread
            && AttachThreadInput(cur_thread, fg_thread, 1) != 0;
        let fg2 = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(cur_thread, fg_thread, 0);
        }

        // それでも前に出ない場合のフォールバック。
        // 昇格ウィンドウは非昇格の前面ウィンドウから SetForegroundWindow で
        // アクティブ化できない (Windows の制限)。数秒だけ最前面に固定して
        // ユーザーがクリックできるようにする。
        let top = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        if top != 0 {
            let hwnd_val = hwnd as isize;
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let _ = SetWindowPos(
                    hwnd_val as HWND,
                    HWND_NOTOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE,
                );
            });
        }
        // タスクバーボタンを点滅させて気付かせる
        let fwi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        let _ = FlashWindowEx(&fwi);

        SwitchToThisWindow(hwnd, 1);
        let fg3 = SetForegroundWindow(hwnd);

        focus_log(&format!(
            "hwnd={hwnd:?} fg_thread={fg_thread} attached={attached} z={z} fg1={fg1} fg2={fg2} top={top} fg3={fg3}"
        ));
    }
}

unsafe extern "system" fn enum_focus_proc(hwnd: Handle, _lparam: isize) -> i32 {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len > 0 {
            let mut title = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            let text = String::from_utf16_lossy(&title);
            if text.trim_end_matches('\0') == "MiniTask" {
                bring_to_front(hwnd);
                return 0;
            }
        }
        1
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 現在のプロセスが管理者として昇格しているか。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// 自分自身を UAC 昇格して起動し直す。
pub fn relaunch_elevated() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_w = to_wide(&exe.to_string_lossy());
    let verb = to_wide("runas");
    let params: Vec<String> = std::env::args().skip(1).collect();
    let params_w = to_wide(&params.join(" "));
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            params_w.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        Err(format!("昇格起動に失敗しました (code {})", result as isize))
    } else {
        Ok(())
    }
}

fn notify_tray_show() -> bool {
    // トレイ常駐プロセスの egui 側に Visible(true) を通知する
    unsafe {
        let class: Vec<u16> = crate::tray::CLASS_NAME.encode_utf16().collect();
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() {
            false
        } else {
            PostMessageW(hwnd, crate::tray::WM_TRAY_SHOW, 0, 0) != 0
        }
    }
}

/// 既存インスタンスに「表示して」と通知できたら true。
pub fn notify_existing_instance() -> bool {
    unsafe {
        // ホットキー経由で起動された場合、このプロセスはフォアグラウンド権を
        // 持っていることがある。既存の昇格プロセスが前面に出られるよう委譲する。
        let allowed = AllowSetForegroundWindow(ASFW_ANY);
        focus_log(&format!("allow_set_foreground={allowed}"));
    }
    // 通知側からも直接前面化を試みる (昇格先の SetForegroundWindow が
    // フォアグラウンドロックで失敗する場合の保険)。
    focus_window();
    let posted = notify_tray_show();
    if posted {
        // AllowSetForegroundWindow の許可は呼び出し元の終了で失効するため、
        // 既存側が前面化を終えるまで少しだけ待つ。
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
    posted
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
            if text.trim_end_matches('\0') == "MiniTask" {
                bring_to_front(hwnd);
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
    if found {
        notify_tray_show();
    }
    found
}

/// ウィンドウ全体の不透明度を維持する。スタイルが上書きされた場合は再適用する。
/// 毎フレーム呼び出しても、WS_EX_LAYERED が付いていれば何もしない。
pub fn ensure_window_opacity(percent: u8) {
    if percent >= 100 {
        return;
    }
    struct OpacityCtx {
        alpha: u8,
    }
    unsafe extern "system" fn proc(hwnd: Handle, lparam: isize) -> i32 {
        unsafe {
            let alpha = (*(lparam as *const OpacityCtx)).alpha;
            let len = GetWindowTextLengthW(hwnd);
            if len > 0 {
                let mut title = vec![0u16; (len + 1) as usize];
                GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
                let text = String::from_utf16_lossy(&title);
                if text.trim_end_matches('\0') == "MiniTask" {
                    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                    if ex & WS_EX_LAYERED as isize == 0 {
                        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
                    }
                    SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
                    return 0;
                }
            }
            1
        }
    }
    let ctx = OpacityCtx {
        alpha: (percent as f32 * 2.55).round().min(255.0) as u8,
    };
    unsafe {
        EnumWindows(Some(proc), &ctx as *const OpacityCtx as isize);
    }
}

/// メインウィンドウが最小化されていたらトレイへ格納する。
/// 最小化中は egui の update が呼ばれないため、常駐スレッドから直接実行する。
pub fn try_hide_minimized_window() {
    unsafe extern "system" fn proc(hwnd: Handle, _lparam: isize) -> i32 {
        unsafe {
            let len = GetWindowTextLengthW(hwnd);
            if len > 0 {
                let mut title = vec![0u16; (len + 1) as usize];
                GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
                let text = String::from_utf16_lossy(&title);
                if text.trim_end_matches('\0') == "MiniTask" {
                    if IsIconic(hwnd) != 0 && IsWindowVisible(hwnd) != 0 {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                    return 0;
                }
            }
            1
        }
    }
    unsafe {
        EnumWindows(Some(proc), 0);
    }
}

// ---------------------------------------------------------------------------
// Windows サインイン時の自動起動 (HKCU\...\Run)
// ---------------------------------------------------------------------------

const AUTOSTART_RUN_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_VALUE_NAME: &str = "MiniTask";

pub fn set_autostart(enable: bool, exe_path: &str) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if enable {
        let (key, _) = hkcu
            .create_subkey_with_flags(AUTOSTART_RUN_PATH, KEY_SET_VALUE | KEY_WOW64_64KEY)
            .map_err(|e| format!("自動起動キーを開けません: {e}"))?;
        key.set_value(AUTOSTART_VALUE_NAME, &format!("\"{exe_path}\""))
            .map_err(|e| format!("自動起動値を設定できません: {e}"))?;
    } else {
        let key = hkcu
            .open_subkey_with_flags(AUTOSTART_RUN_PATH, KEY_SET_VALUE | KEY_WOW64_64KEY)
            .map_err(|e| format!("自動起動キーを開けません: {e}"))?;
        // 値が無い場合は成功扱い
        match key.delete_value(AUTOSTART_VALUE_NAME) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("自動起動値を削除できません: {e}")),
        }
    }
    Ok(())
}

pub fn is_autostart_active() -> bool {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(AUTOSTART_RUN_PATH, KEY_READ | KEY_WOW64_64KEY)
    else {
        return false;
    };
    key.get_value::<String, _>(AUTOSTART_VALUE_NAME).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifeo_constants() {
        assert!(IFEO_PATH.contains("Image File Execution Options"));
    }
}
