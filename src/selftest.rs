//! ヘッドレスセルフテスト (MINITASK_SELFTEST=1 で起動)。
//! GUI を開かずに主要機能を検証する。管理者権限で実行すること。

use std::io::Write;
use std::time::Duration;

use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

use crate::net::NetSession;
use crate::process::{self, ProcCollector};
use crate::settings::AppSettings;
use crate::startup;

const RUN_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const TEST_NAME: &str = "MiniTaskSelfTest";

pub fn run() -> i32 {
    let mut failed = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("[{}] {name}: {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    };

    // 1. プロセス列挙
    let mut collector = match ProcCollector::new() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] collector init: {e}");
            return 1;
        }
    };
    let (rows, load) = collector.refresh();
    check(
        "process enumeration",
        rows.len() > 10,
        format!("{} processes, cpu={:.1}%, mem={}/{}", rows.len(), load.cpu, load.mem_used, load.mem_total),
    );
    check(
        "performance metrics",
        (0.0..=100.0).contains(&load.cpu)
            && (0.0..=100.0).contains(&load.rom)
            && (0.0..=100.0).contains(&load.gpu)
            && (0.0..=100.0).contains(&load.npu)
            && !load.core_usage.is_empty()
            && load.core_usage.iter().all(|c| (0.0..=100.0).contains(c))
            && !load.gpu_engines.is_empty()
            && load.gpu_engines.iter().all(|(_, v)| (0.0..=100.0).contains(v)),
        format!(
            "cpu={:.1}%, rom={:.1}%, gpu={:.1}%, npu={:.1}%, cores={}, gpu_engines={} (counter={}, npu_device={})",
            load.cpu, load.rom, load.gpu, load.npu, load.core_usage.len(), load.gpu_engines.len(), load.npu_available, load.npu_device_present
        ),
    );
    let settings = AppSettings::load();
    check(
        "settings persistence",
        [500, 1000, 2000, 5000].contains(&settings.interval_ms)
            && settings.sort_col < 14
            && settings.tab <= 2
            && settings.perf_kind <= 5
            && settings.same(&AppSettings::load()),
        format!(
            "interval={}ms tab={} perf={}:{} cols={:014b} anim={} spline={}",
            settings.interval_ms,
            settings.tab,
            settings.perf_kind,
            settings.perf_id,
            settings.columns_mask,
            settings.graph_animation,
            settings.graph_spline
        ),
    );
    check(
        "device enumeration",
        !load.disks.is_empty()
            && load.disks.iter().all(|d| !d.id.is_empty())
            && load
                .gpus
                .iter()
                .all(|g| !g.id.is_empty() && (0.0..=100.0).contains(&g.usage))
            && load
                .npus
                .iter()
                .all(|n| !n.id.is_empty() && (0.0..=100.0).contains(&n.usage))
            && !load.adapters.is_empty()
            && load.adapters.iter().all(|a| !a.id.is_empty()),
        format!(
            "disks=[{}] gpus=[{}] npus=[{}] adapters=[{}]",
            load.disks
                .iter()
                .map(|d| d.label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            load.gpus
                .iter()
                .map(|g| g.label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            load.npus
                .iter()
                .map(|n| n.label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            load.adapters
                .iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    );
    for d in &load.disks {
        println!(
            "  Disk {}: read={}/s write={}/s",
            d.label,
            crate::util::fmt_rate(d.read_bps),
            crate::util::fmt_rate(d.write_bps)
        );
    }
    for a in &load.adapters {
        println!(
            "  Adapter {}: recv={}/s sent={}/s wifi={} ssid={} signal={}% ipv4={:?}",
            a.label,
            crate::util::fmt_rate(a.recv_bps),
            crate::util::fmt_rate(a.sent_bps),
            a.is_wifi,
            a.ssid,
            a.signal_pct,
            a.ipv4,
        );
    }
    for (key, val) in load.gpu_engines.iter().take(8) {
        println!("  GPU engine: {} = {:.1}%", crate::perf::engine_display_name(key), val);
    }
    for (key, val) in load.npu_engines.iter().take(4) {
        println!("  NPU engine: {} = {:.1}%", crate::perf::npu_engine_display_name(key), val);
    }
    let self_row = rows.iter().find(|r| r.pid == std::process::id());
    check(
        "self process info",
        self_row.is_some_and(|r| !r.name.is_empty() && r.mem > 0 && !r.user.starts_with("S-1-")),
        format!("{:?}", self_row.map(|r| (&r.name, r.mem, &r.user))),
    );

    // 2. ETW セッション
    let mut net = match NetSession::start() {
        Ok(n) => {
            check("etw session", true, "started".into());
            Some(n)
        }
        Err(e) => {
            check("etw session", false, e);
            None
        }
    };

    // 3. プロセス終了 (ダミーの cmd を起動して終了。notepad は Win11 で
    //    単一インスタンス化のためすぐ終了してしまいテストにならない)
    let dummy = std::process::Command::new("cmd.exe")
        .args(["/c", "ping -n 30 127.0.0.1 >nul"])
        .spawn();
    match dummy {
        Ok(mut child) => {
            let pid = child.id();
            std::thread::sleep(Duration::from_millis(800));
            let kill_result = process::kill(pid);
            let _ = child.wait();
            let gone = !pid_alive(pid);
            check(
                "process kill",
                kill_result.is_ok() && gone,
                format!("kill={:?}, pid={pid} gone={gone}", kill_result),
            );
        }
        Err(e) => check("process kill", false, format!("dummy spawn failed: {e}")),
    }

    // 3b. プロセス ツリー終了 (cmd → 子 cmd を終了)
    let dummy = std::process::Command::new("cmd.exe")
        .args(["/c", "cmd /c ping -n 30 127.0.0.1 >nul"])
        .spawn();
    match dummy {
        Ok(mut child) => {
            let pid = child.id();
            std::thread::sleep(Duration::from_millis(1000));
            // ツリー情報を取得して kill_tree
            let (rows, _) = collector.refresh();
            let kill_result = process::kill_tree(pid, &rows);
            let _ = child.wait();
            let gone = !pid_alive(pid);
            check(
                "process tree kill",
                kill_result.is_ok() && gone,
                format!("kill={:?}, pid={pid} gone={gone}", kill_result),
            );
        }
        Err(e) => check("process tree kill", false, format!("dummy spawn failed: {e}")),
    }

    // 4. スタートアップ管理 (HKCU Run にテスト値を作成 → 無効化 → 有効化 → 削除)
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let setup = hkcu
        .open_subkey_with_flags(RUN_PATH, KEY_SET_VALUE)
        .and_then(|k| k.set_value(TEST_NAME, &"C:\\Windows\\System32\\notepad.exe"));
    if let Err(e) = setup {
        check("startup create test value", false, e.to_string());
    } else {
        let entries = startup::list();
        let found = entries.iter().find(|e| e.name == TEST_NAME);
        check(
            "startup list",
            found.is_some_and(|e| e.enabled && e.command.contains("notepad")),
            format!("{found:?}"),
        );
        if let Some(entry) = found {
            let r1 = startup::set_enabled(entry, false);
            let e1 = startup::list().into_iter().find(|e| e.name == TEST_NAME);
            check(
                "startup disable",
                r1.is_ok() && e1.as_ref().is_some_and(|e| !e.enabled),
                format!("result={r1:?}, enabled={:?}", e1.as_ref().map(|e| e.enabled)),
            );
            if let Some(e1) = e1 {
                let r2 = startup::set_enabled(&e1, true);
                let e2 = startup::list().into_iter().find(|e| e.name == TEST_NAME);
                check(
                    "startup enable",
                    r2.is_ok() && e2.as_ref().is_some_and(|e| e.enabled),
                    format!("result={r2:?}, enabled={:?}", e2.as_ref().map(|e| e.enabled)),
                );
            }
            let r3 = startup::delete(entry);
            let e3 = startup::list().into_iter().find(|e| e.name == TEST_NAME);
            check(
                "startup delete",
                r3.is_ok() && e3.is_none(),
                format!("result={r3:?}, still_present={}", e3.is_some()),
            );
        }
        // 承認キーのテスト値も掃除
        if let Ok(k) = hkcu.open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
            KEY_SET_VALUE,
        ) {
            let _ = k.delete_value(TEST_NAME);
        }
    }

    // 5. ネットワークカウンタ (TCP 接続 + 送信でカーネルイベントを発生させる)
    if net.is_some() {
        let before = crate::net::snapshot();
        let total_before: u64 = before.values().map(|c| c.sent + c.recv).sum();
        let mut traffic = Ok(());
        // 数回送信しておく (FlushTimer=1s のため反映に最大数秒かかる)
        for _ in 0..3 {
            traffic = std::net::TcpStream::connect("static.crates.io:443").and_then(|mut s| {
                s.set_write_timeout(Some(Duration::from_secs(5)))?;
                // TLS ClientHello 相当のバイト列を送信 (TcpIp_Send イベントが発生する)
                s.write_all(&[0x16, 0x03, 0x01, 0x00, 0x40, 0x01, 0x00, 0x00, 0x3c, 0x03, 0x03])?;
                s.write_all(&[0u8; 64])?;
                s.flush()
            });
            if traffic.is_err() {
                break;
            }
        }
        // 最大 8 秒ポーリングしてカウンタ増加を待つ
        let mut total_after = total_before;
        for _ in 0..16 {
            std::thread::sleep(Duration::from_millis(500));
            total_after = crate::net::snapshot().values().map(|c| c.sent + c.recv).sum();
            if total_after > total_before {
                break;
            }
        }
        check(
            "etw counters increase",
            traffic.is_ok() && total_after > total_before,
            format!("traffic={traffic:?}, before={total_before}, after={total_after}"),
        );
        if let Some(n) = net.as_mut() {
            n.stop();
        }
    }

    println!("=== selftest finished: {} failure(s) ===", failed);
    if failed == 0 { 0 } else { 1 }
}

fn pid_alive(pid: u32) -> bool {
    unsafe {
        let h = windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = windows_sys::Win32::System::Threading::GetExitCodeProcess(h, &mut code);
        windows_sys::Win32::Foundation::CloseHandle(h);
        // STILL_ACTIVE (259) なら生存
        ok != 0 && code == 259
    }
}
