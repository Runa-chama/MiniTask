//! MiniTask — 軽量タスクマネージャー (Windows 10/11)
//!
//! - プロセス一覧 / 強制終了
//! - CPU / メモリ / ディスク I/O / プロセス別ネットワーク使用量 (ETW)
//! - スタートアップ管理
//!
//! 管理者権限で実行される (app.manifest の requireAdministrator)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod net;
mod netadapter;
mod perf;
mod process;
mod selftest;
mod startup;
mod util;

fn main() -> eframe::Result {
    if std::env::var("MINITASK_SELFTEST").is_ok() {
        std::process::exit(selftest::run());
    }

    // 他ユーザープロセス等の照会・終了のため SeDebugPrivilege を有効化
    process::enable_debug_privilege();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1020.0, 660.0])
            .with_min_inner_size([760.0, 440.0])
            .with_maximized(std::env::var("MINITASK_MAXIMIZE").is_ok())
            .with_title("MiniTask — 軽量タスクマネージャー"),
        ..Default::default()
    };

    eframe::run_native(
        "MiniTask",
        options,
        Box::new(|cc| {
            util::setup_fonts(&cc.egui_ctx);
            util::setup_theme(&cc.egui_ctx);
            Ok(Box::new(app::MiniTaskApp::new()))
        }),
    )
}
