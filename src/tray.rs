//! タスクトレイ常駐アイコン。
//! 専用スレッドで非表示ウィンドウ＋メッセージループを持ち、
//! Shell_NotifyIconW で通知領域アイコンを管理する。

use std::sync::mpsc::{self, Receiver, Sender};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CW_USEDEFAULT, ChangeWindowMessageFilterEx, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, HICON, HMENU, MF_STRING, MSG,
    MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

use crate::integration;

const WM_TRAYICON: u32 = 0x0400 + 1; // WM_APP + 1
pub const WM_TRAY_SHOW: u32 = 0x0400 + 2; // 他プロセスからの「表示して」要求
const ID_OPEN: usize = 0x4D54_0001;
const ID_EXIT: usize = 0x4D54_0002;
pub const CLASS_NAME: &str = "MiniTaskTrayNotify\0";

#[derive(Debug)]
pub enum TrayMsg {
    Open,
    Exit,
}

struct TrayState {
    tx: Sender<TrayMsg>,
    icon: HICON,
    taskbar_created: u32,
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    if s.ends_with('\0') {
        s.encode_utf16().collect()
    } else {
        to_wide(s)
    }
}

fn load_tray_icon() -> HICON {
    unsafe {
        let exe = std::env::current_exe()
            .ok()
            .map(|p| to_wide(&p.to_string_lossy()))
            .unwrap_or_default();
        let mut large: HICON = std::ptr::null_mut();
        let mut small: HICON = std::ptr::null_mut();
        let n = if !exe.is_empty() {
            ExtractIconExW(exe.as_ptr(), 0, &mut large, &mut small, 1)
        } else {
            0
        };
        if n > 0 && !small.is_null() {
            if !large.is_null() {
                DestroyIcon(large);
            }
            return small;
        }
        if n > 0 && !large.is_null() {
            return large;
        }
        std::ptr::null_mut()
    }
}

fn add_icon(hwnd: HWND, icon: HICON) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = icon;
        let tip = to_wide("MiniTask");
        let n = tip.len().min(nid.szTip.len());
        nid.szTip[..n].copy_from_slice(&tip[..n]);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

/// ツールチップを更新する (アプリ側スレッドから呼び出し可)。
pub fn set_tip(hwnd: isize, tip: &str) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd as HWND;
        nid.uID = 1;
        nid.uFlags = NIF_TIP;
        let text = to_wide(tip);
        let n = text.len().min(nid.szTip.len());
        nid.szTip[..n].copy_from_slice(&text[..n]);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn show_context_menu(hwnd: HWND, tx: &Sender<TrayMsg>) {
    unsafe {
        let menu: HMENU = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let open = to_wide("MiniTask を開く");
        let exit = to_wide("終了");
        AppendMenuW(menu, MF_STRING, ID_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_EXIT, exit.as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);

        // メニューが正しく消えるよう、前面化してから表示・表示後に WM_NULL
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);

        match cmd as usize {
            ID_OPEN => {
                let _ = tx.send(TrayMsg::Open);
                integration::focus_window();
            }
            ID_EXIT => {
                let _ = tx.send(TrayMsg::Exit);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_CREATE {
            let cs = &*(lparam as *const CREATESTRUCTW);
            let state = cs.lpCreateParams as *mut TrayState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            add_icon(hwnd, (*state).icon);
            return 0;
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &*ptr;

        if msg == WM_TRAYICON {
            let ev = lparam as u32;
            if ev == WM_LBUTTONUP || ev == WM_LBUTTONDBLCLK {
                let _ = state.tx.send(TrayMsg::Open);
                integration::focus_window();
                return 0;
            }
            if ev == WM_RBUTTONUP {
                show_context_menu(hwnd, &state.tx);
                return 0;
            }
        }

        if msg == state.taskbar_created && state.taskbar_created != 0 {
            // explorer 再起動後にアイコンを再登録
            add_icon(hwnd, state.icon);
            return 0;
        }

        if msg == WM_TRAY_SHOW {
            let _ = state.tx.send(TrayMsg::Open);
            integration::focus_window();
            return 0;
        }

        if msg == WM_COMMAND {
            // 念のため（TPM_RETURNCMD を使っているので通常ここは来ない）
            match wparam & 0xffff {
                v if v == ID_OPEN => {
                    let _ = state.tx.send(TrayMsg::Open);
                    integration::focus_window();
                }
                v if v == ID_EXIT => {
                    let _ = state.tx.send(TrayMsg::Exit);
                }
                _ => {}
            }
            return 0;
        }

        if msg == WM_CLOSE {
            DestroyWindow(hwnd);
            return 0;
        }

        if msg == WM_DESTROY {
            remove_icon(hwnd);
            PostQuitMessage(0);
            return 0;
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

pub struct TrayHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    hwnd: isize,
}

impl TrayHandle {
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn stop(&mut self) {
        if self.hwnd != 0 {
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.hwnd = 0;
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start() -> (TrayHandle, Receiver<TrayMsg>) {
    let (tx, rx) = mpsc::channel::<TrayMsg>();
    let (ready_tx, ready_rx) = mpsc::channel::<isize>();

    let thread = std::thread::spawn(move || {
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class_name = to_wide_nul(CLASS_NAME);

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            RegisterClassExW(&wc);

            let taskbar_created = RegisterWindowMessageW(to_wide("TaskbarCreated").as_ptr());
            let icon = load_tray_icon();
            let state = Box::new(TrayState {
                tx,
                icon,
                taskbar_created,
            });
            let state_ptr = Box::into_raw(state);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                to_wide("MTTrayNotify").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                state_ptr.cast(),
            );

            if !hwnd.is_null() {
                // 非昇格プロセスからの「表示して」要求を許可する (UIPI 対策)
                ChangeWindowMessageFilterEx(hwnd, WM_TRAY_SHOW, MSGFLT_ALLOW, std::ptr::null_mut());
            }
            let _ = ready_tx.send(hwnd as isize);

            if hwnd.is_null() {
                // state を回収
                let state = Box::from_raw(state_ptr);
                if !state.icon.is_null() {
                    DestroyIcon(state.icon);
                }
                return;
            }

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // ループ終了後: WM_DESTROY でアイコン削除済み。リソース解放のみ。
            if !(*state_ptr).icon.is_null() {
                DestroyIcon((*state_ptr).icon);
            }
            drop(Box::from_raw(state_ptr));
        }
    });

    let hwnd = ready_rx.recv().unwrap_or(0);
    (
        TrayHandle {
            thread: Some(thread),
            hwnd,
        },
        rx,
    )
}
