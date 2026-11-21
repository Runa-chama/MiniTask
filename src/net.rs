//! ETW (Event Tracing for Windows) によるプロセス別ネットワーク使用量計測。
//!
//! Microsoft-Windows-Kernel-Network プロバイダーのリアルタイムセッションを開き、
//! TCP/UDP の Send (opcode 10) / Recv (opcode 11) イベントから
//! PID 別の送受信バイト数を累積する。管理者権限が必要。
//!
//! イベントペイロード先頭: u32 PID, u32 size (送信/受信バイト数)。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_VERBOSE,
    WNODE_FLAG_TRACED_GUID,
};

const SESSION_NAME: &str = "MiniTaskNetTrace";

/// Microsoft-Windows-Kernel-Network {7DD42A49-5329-4832-8DFD-43D979153A88}
const PROVIDER: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);

const OP_SEND: u8 = 10;
const OP_RECV: u8 = 11;

#[derive(Clone, Copy, Default, Debug)]
pub struct NetCounters {
    pub sent: u64,
    pub recv: u64,
}

static COUNTERS: OnceLock<Mutex<HashMap<u32, NetCounters>>> = OnceLock::new();

fn counters() -> &'static Mutex<HashMap<u32, NetCounters>> {
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 現在の PID 別累積カウンタのスナップショット。
pub fn snapshot() -> HashMap<u32, NetCounters> {
    counters().lock().unwrap().clone()
}

/// 終了したプロセスのカウンタを破棄してメモリ増加を防ぐ。
pub fn prune(live_pids: &HashSet<u32>) {
    counters().lock().unwrap().retain(|pid, _| live_pids.contains(pid));
}

#[inline]
fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// ETW コールバック (専用スレッドから呼ばれる)。
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    unsafe {
        let rec = &*record;
        if !same_guid(&rec.EventHeader.ProviderId, &PROVIDER) {
            return;
        }
        let opcode = rec.EventHeader.EventDescriptor.Opcode;
        if opcode != OP_SEND && opcode != OP_RECV {
            return;
        }
        if rec.UserDataLength < 8 || rec.UserData.is_null() {
            return;
        }
        let pid = (rec.UserData as *const u32).read_unaligned();
        let size = (rec.UserData as *const u8).add(4).cast::<u32>().read_unaligned() as u64;
        let mut map = counters().lock().unwrap();
        let entry = map.entry(pid).or_default();
        if opcode == OP_SEND {
            entry.sent = entry.sent.saturating_add(size);
        } else {
            entry.recv = entry.recv.saturating_add(size);
        }
    }
}

/// セッション名入りの EVENT_TRACE_PROPERTIES バッファを作る。
fn make_props_buffer(name_w: &[u16]) -> Vec<u8> {
    let struct_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let mut buf = vec![0u8; struct_size + name_w.len() * 2];
    unsafe {
        let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        (*props).Wnode.BufferSize = buf.len() as u32;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).Wnode.ClientContext = 1; // QPC タイムスタンプ
        (*props).BufferSize = 64; // KB
        (*props).MinimumBuffers = 2;
        (*props).MaximumBuffers = 16;
        (*props).FlushTimer = 1; // 秒
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).LoggerNameOffset = struct_size as u32;
        std::ptr::copy_nonoverlapping(
            name_w.as_ptr(),
            buf.as_mut_ptr().add(struct_size).cast::<u16>(),
            name_w.len(),
        );
    }
    buf
}

pub struct NetSession {
    handle: CONTROLTRACE_HANDLE,
    thread: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl NetSession {
    /// ETW リアルタイムセッションを開始する。失敗時は日本語エラーメッセージ。
    pub fn start() -> Result<Self, String> {
        let name_w: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = make_props_buffer(&name_w);
        let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();

        unsafe {
            let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
            let mut status = StartTraceW(&mut handle, name_w.as_ptr(), props);
            if status == ERROR_ALREADY_EXISTS {
                // 前回の異常終了で残ったセッションを停止して再試行
                let _ = ControlTraceW(
                    CONTROLTRACE_HANDLE { Value: 0 },
                    name_w.as_ptr(),
                    props,
                    EVENT_TRACE_CONTROL_STOP,
                );
                status = StartTraceW(&mut handle, name_w.as_ptr(), props);
            }
            if status != ERROR_SUCCESS {
                let hint = if status == ERROR_ACCESS_DENIED {
                    " (管理者権限で実行してください)"
                } else {
                    ""
                };
                return Err(format!("ETW セッション開始失敗: コード {status}{hint}"));
            }

            let status = EnableTraceEx2(
                handle,
                &PROVIDER,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                TRACE_LEVEL_VERBOSE as u8,
                u64::MAX, // すべてのキーワード
                0,
                0,
                std::ptr::null(),
            );
            if status != ERROR_SUCCESS {
                let _ = ControlTraceW(handle, std::ptr::null(), props, EVENT_TRACE_CONTROL_STOP);
                return Err(format!("ETW プロバイダー有効化失敗: コード {status}"));
            }

            // リアルタイム消費者を開く
            let mut logfile = EVENT_TRACE_LOGFILEW::default();
            logfile.LoggerName = name_w.as_ptr() as *mut u16;
            logfile.Anonymous1.ProcessTraceMode =
                PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
            logfile.Anonymous2.EventRecordCallback = Some(on_event);
            let trace_handle = OpenTraceW(&mut logfile);
            if trace_handle.Value == u64::MAX {
                let _ = ControlTraceW(handle, std::ptr::null(), props, EVENT_TRACE_CONTROL_STOP);
                return Err("ETW リアルタイム消費の開始に失敗しました".to_owned());
            }

            let alive = Arc::new(AtomicBool::new(true));
            let alive2 = alive.clone();
            let thread = match std::thread::Builder::new()
                .name("minitask-etw".to_owned())
                .spawn(move || {
                    // セッションが停止されるまでブロックする
                    let _ = ProcessTrace(&trace_handle, 1, std::ptr::null(), std::ptr::null());
                    let _ = CloseTrace(trace_handle);
                    alive2.store(false, Ordering::SeqCst);
                }) {
                Ok(t) => t,
                Err(e) => {
                    let _ = ControlTraceW(handle, std::ptr::null(), props, EVENT_TRACE_CONTROL_STOP);
                    return Err(format!("ETW スレッド起動失敗: {e}"));
                }
            };

            Ok(Self {
                handle,
                thread: Some(thread),
                alive,
            })
        }
    }

    /// セッションを停止し、消費スレッドの終了を待つ。
    pub fn stop(&mut self) {
        if self.handle.Value != 0 {
            let name_w: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
            let mut buf = make_props_buffer(&name_w);
            unsafe {
                let _ = ControlTraceW(
                    self.handle,
                    std::ptr::null(),
                    buf.as_mut_ptr().cast(),
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            self.handle.Value = 0;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for NetSession {
    fn drop(&mut self) {
        self.stop();
    }
}
