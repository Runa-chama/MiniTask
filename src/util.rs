//! 見た目・フォーマット系のユーティリティ。

use std::collections::VecDeque;

use egui::{Color32, Pos2, Vec2};
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows_sys::Win32::System::Time::FileTimeToSystemTime;

pub const ACCENT: Color32 = Color32::from_rgb(76, 194, 255); // Win11 ダークアクセント
pub const CPU_COLOR: Color32 = Color32::from_rgb(96, 205, 255);
pub const MEM_COLOR: Color32 = Color32::from_rgb(108, 203, 95);
pub const ROM_COLOR: Color32 = Color32::from_rgb(230, 170, 110);
pub const GPU_COLOR: Color32 = Color32::from_rgb(170, 145, 255);
pub const NPU_COLOR: Color32 = Color32::from_rgb(255, 150, 185);
pub const WARN: Color32 = Color32::from_rgb(255, 190, 90);
pub const DANGER: Color32 = Color32::from_rgb(255, 120, 100);

/// 日本語フォントをシステムフォントから読み込む。
/// egui 同梱フォントには日本語グリフが無いため必須。
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // TTC の index 0 を使用 (Meiryo Regular / Yu Gothic Regular / MS Gothic)
    for file in ["meiryo.ttc", "YuGothR.ttc", "msgothic.ttc"] {
        let path = format!("{}\\{}", std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()), format!("Fonts\\{file}"));
        if let Ok(bytes) = std::fs::read(&path) {
            fonts
                .font_data
                .insert("jp".to_owned(), egui::FontData::from_owned(bytes).into());
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.insert(0, "jp".to_owned());
            }
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.push("jp".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// Win11 風ダークテーマ。
pub fn setup_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.global_style()).clone();
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(25, 27, 33);
    v.window_fill = Color32::from_rgb(32, 34, 42);
    v.extreme_bg_color = Color32::from_rgb(17, 18, 23);
    v.faint_bg_color = Color32::from_rgb(30, 33, 41);
    v.override_text_color = Some(Color32::from_rgb(228, 230, 235));
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(0, 90, 158);
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.menu_corner_radius = egui::CornerRadius::same(8);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::same(6);
    }
    v.widgets.noninteractive.bg_fill = Color32::from_rgb(32, 34, 42);
    v.widgets.inactive.bg_fill = Color32::from_rgb(45, 48, 58);
    v.widgets.hovered.bg_fill = Color32::from_rgb(58, 62, 74);
    v.widgets.active.bg_fill = Color32::from_rgb(0, 90, 158);
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    ctx.set_global_style(style);
}

/// バイト量を見やすい単位に (メモリ量など)。
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 転送速度 (bytes/sec) を見やすい単位に。0 は "—"。
pub fn fmt_rate(bps: u64) -> String {
    if bps == 0 {
        return "—".to_owned();
    }
    format!("{}/s", fmt_bytes(bps))
}

/// UNIX 秒をローカル時刻の "YYYY/MM/DD HH:MM" に。
pub fn fmt_time(unix_secs: u64) -> String {
    if unix_secs == 0 {
        return "—".to_owned();
    }
    unsafe {
        // UNIX epoch → FILETIME (1601-01-01 からの 100ns 単位)
        let ticks = (unix_secs + 11_644_473_600).saturating_mul(10_000_000);
        let ft = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut local: FILETIME = std::mem::zeroed();
        let mut st: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToLocalFileTime(&ft, &mut local) != 0 && FileTimeToSystemTime(&local, &mut st) != 0 {
            format!(
                "{:04}/{:02}/{:02} {:02}:{:02}",
                st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
            )
        } else {
            "—".to_owned()
        }
    }
}

/// 小さな折れ線グラフ (CPU / メモリ履歴用)。値は 0..max の百分率。
pub fn sparkline(
    ui: &mut egui::Ui,
    values: &VecDeque<f32>,
    max: f32,
    color: Color32,
    size: Vec2,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        let n = values.len();
        if n >= 2 {
            let inner_h = rect.height() - 4.0;
            let pts: Vec<Pos2> = values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let x = rect.left() + 2.0 + (i as f32 / (n - 1) as f32) * (rect.width() - 4.0);
                    let y = rect.bottom() - 2.0 - (v / max).clamp(0.0, 1.0) * inner_h;
                    Pos2::new(x, y)
                })
                .collect();
            // 曲線の下を薄く塗る
            let mut fill = pts.clone();
            fill.push(Pos2::new(rect.right() - 2.0, rect.bottom() - 2.0));
            fill.push(Pos2::new(rect.left() + 2.0, rect.bottom() - 2.0));
            painter.add(egui::epaint::PathShape {
                points: fill,
                closed: true,
                fill: color.gamma_multiply(0.18),
                stroke: egui::epaint::PathStroke::NONE,
            });
            painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, color)));
        }
    }
    resp
}

/// 数値セルを右揃え・等幅で表示。
pub fn num_cell(ui: &mut egui::Ui, text: impl Into<String>, color: Option<Color32>) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let mut rt = egui::RichText::new(text).monospace();
        if let Some(c) = color {
            rt = rt.color(c);
        }
        ui.label(rt);
    });
}

/// CPU 使用率に応じた色。
pub fn cpu_color(pct: f32) -> Option<Color32> {
    if pct >= 80.0 {
        Some(DANGER)
    } else if pct >= 40.0 {
        Some(WARN)
    } else {
        None
    }
}
