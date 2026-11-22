//! MiniTask — 軽量タスクマネージャー (Windows 10/11)
//!
//! - プロセス一覧 / 強制終了
//! - CPU / メモリ / ディスク I/O / プロセス別ネットワーク使用量 (ETW)
//! - スタートアップ管理
//!
//! app.manifest は asInvoker。起動時に管理者へ自己昇格する
//! (IFEO の CreateProcess 起動でも error 740 にならないようにするため)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod icon;
mod integration;
mod net;
mod netadapter;
mod perf;
mod process;
mod selftest;
mod settings;
mod smart;
mod startup;
mod tray;
mod update;
mod util;

fn main() -> eframe::Result {
    if std::env::var("MINITASK_SELFTEST").is_ok() {
        std::process::exit(selftest::run());
    }

    let allow_multi = std::env::var("MINITASK_ALLOW_MULTI").is_ok();

    // IFEO は CreateProcess で起動するため、requireAdministrator だと
    // error 740 で何も起動しない。非昇格で起動した場合は必要に応じて自己昇格する。
    // 既存インスタンスがあれば昇格せずに通知だけ行う (UAC を出さない)。
    if !integration::is_elevated() {
        if !allow_multi && integration::notify_existing_instance() {
            return Ok(());
        }
        if integration::relaunch_elevated().is_ok() {
            return Ok(());
        }
    }

    // 多重起動防止: 既存インスタンスがあれば前面化して終了
    if !allow_multi && integration::focus_existing_instance() {
        return Ok(());
    }

    // 他ユーザープロセス等の照会・終了のため SeDebugPrivilege を有効化
    process::enable_debug_privilege();

    // MINITASK_SIZE=幅x高さ で起動サイズを指定可能 (検証用)
    let initial_size: [f32; 2] = std::env::var("MINITASK_SIZE")
        .ok()
        .and_then(|s| {
            let mut parts = s.split('x');
            let w: f32 = parts.next()?.parse().ok()?;
            let h: f32 = parts.next()?.parse().ok()?;
            Some([w, h])
        })
        .unwrap_or([1020.0, 660.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(initial_size)
        .with_min_inner_size([760.0, 440.0])
        .with_maximized(std::env::var("MINITASK_MAXIMIZE").is_ok())
        .with_title("MiniTask — 軽量タスクマネージャー");
    if let Some(data) = icon::window_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(data));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    // 起動直後にウィンドウを前面に出す。IFEO (Ctrl+Shift+Esc) 起動では
    // 昇格の関係で自動フォーカスが効かない場合があるため、最前面表示と
    // タスクバー点滅で気付けるようにする。
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        integration::focus_window();
    });

    eframe::run_native(
        "MiniTask",
        options,
        Box::new(|cc| {
            util::setup_fonts(&cc.egui_ctx);
            Ok(Box::new(app::MiniTaskApp::new(&cc.egui_ctx)))
        }),
    )
}
