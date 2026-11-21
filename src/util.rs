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
/// ネットワーク受信 (本家の緑)
pub const NET_RECV_COLOR: Color32 = Color32::from_rgb(108, 203, 95);
/// ネットワーク送信 (本家の黄)
pub const NET_SEND_COLOR: Color32 = Color32::from_rgb(230, 200, 90);
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
/// タスクマネージャーと同じ「KB/秒」表記。
pub fn fmt_rate(bps: u64) -> String {
    if bps == 0 {
        return "—".to_owned();
    }
    format!("{}/秒", fmt_bytes(bps))
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

/// グラフ履歴の最大サンプル数。すべてのグラフで共有する。
pub const HISTORY_LEN: usize = 120;

/// グラフ描画オプション (アニメーション位相と補間方式)。
#[derive(Clone, Copy, Default)]
pub struct GraphStyle {
    /// 更新間隔内の進み具合 (0..1)。折れ線全体を左へずらす
    pub phase: f32,
    /// true = Catmull-Rom スプライン、false = 折れ線
    pub spline: bool,
}

/// 折れ線グラフの共通描画処理 (矩形内に曲線を描く。塗りつぶしなし)。
///
/// タスクマネージャーと同じ挙動: サンプル数が最大に満たない間は
/// 左端から 1 サンプル = 幅/最大サンプル数 ずつ伸びていき、
/// 右側は空いたままになる (全体に引き伸ばさない)。
fn paint_curve(
    painter: &egui::Painter,
    rect: egui::Rect,
    values: &VecDeque<f32>,
    max: f32,
    color: Color32,
    stroke_width: f32,
    style: GraphStyle,
) {
    let n = values.len();
    if n < 2 {
        return;
    }
    let inner_h = rect.height() - 4.0;
    let slot = (rect.width() - 4.0) / HISTORY_LEN.max(n) as f32;
    let pts: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            // phase (0..1) だけ左へずらすことで、更新間隔の間も滑らかに流れる
            let x = rect.left() + 2.0 + i as f32 * slot - style.phase * slot;
            let y = rect.bottom() - 2.0 - (v / max).clamp(0.0, 1.0) * inner_h;
            Pos2::new(x, y)
        })
        .collect();
    let pts = if style.spline && pts.len() >= 3 {
        catmull_rom(&pts, 8)
    } else {
        pts
    };
    let painter = painter.with_clip_rect(rect);
    painter.add(egui::Shape::line(pts, egui::Stroke::new(stroke_width, color)));
}

/// Catmull-Rom 補間でサンプル点間を滑らかな曲線にする。
fn catmull_rom(points: &[Pos2], steps: usize) -> Vec<Pos2> {
    let last = points.len() - 1;
    let mut out = Vec::with_capacity(last * steps + 1);
    out.push(points[0]);
    for i in 0..last {
        let p0 = points[i.saturating_sub(1)];
        let p1 = points[i];
        let p2 = points[i + 1];
        let p3 = points[(i + 2).min(last)];
        for s in 1..=steps {
            let t = s as f32 / steps as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let interpolate = |a: f32, b: f32, c: f32, d: f32| {
                0.5
                    * (2.0 * b
                        + (-a + c) * t
                        + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                        + (-a + 3.0 * b - 3.0 * c + d) * t3)
            };
            out.push(Pos2::new(
                interpolate(p0.x, p1.x, p2.x, p3.x),
                interpolate(p0.y, p1.y, p2.y, p3.y),
            ));
        }
    }
    out
}

/// 2 本の折れ線を同じスケールで重ねて描く (ディスクの読み取り/書き込み)。
pub fn paint_dual_curve(
    painter: &egui::Painter,
    rect: egui::Rect,
    read: &VecDeque<f32>,
    write: &VecDeque<f32>,
    max: f32,
    read_color: Color32,
    write_color: Color32,
    style: GraphStyle,
) {
    paint_curve(painter, rect, read, max, read_color, 1.6, style);
    paint_curve(painter, rect, write, max, write_color, 1.6, style);
}

/// 既に確保済みの矩形内に読み取り/書き込みグラフを描く。
pub fn paint_mini_dual_graph(
    painter: &egui::Painter,
    rect: egui::Rect,
    read: &VecDeque<f32>,
    write: &VecDeque<f32>,
    max: f32,
    read_color: Color32,
    write_color: Color32,
    style: GraphStyle,
) {
    paint_dual_curve(painter, rect, read, write, max, read_color, write_color, style);
}

/// 2 本線の大きなグラフ。25/50/75% のグリッド線付き。
pub fn perf_dual_chart(
    ui: &mut egui::Ui,
    read: &VecDeque<f32>,
    write: &VecDeque<f32>,
    max: f32,
    read_color: Color32,
    write_color: Color32,
    size: Vec2,
    style: GraphStyle,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
        let grid = Color32::from_gray(45);
        for f in [0.25, 0.5, 0.75] {
            let y = rect.bottom() - 2.0 - f * (rect.height() - 4.0);
            painter.line_segment(
                [Pos2::new(rect.left() + 2.0, y), Pos2::new(rect.right() - 2.0, y)],
                egui::Stroke::new(1.0, grid),
            );
        }
        paint_curve(painter, rect, read, max, read_color, 2.0, style);
        paint_curve(painter, rect, write, max, write_color, 2.0, style);
        let stamp = Color32::from_gray(110);
        let font = egui::FontId::proportional(10.0);
        painter.text(
            egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            "30 秒",
            font.clone(),
            stamp,
        );
        painter.text(
            egui::pos2(rect.right() - 4.0, rect.bottom() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            "0",
            font,
            stamp,
        );
    }
    resp
}

/// 履歴のピークから読みやすい上限値 (1/2/5 × 10^n) を決める。
pub fn nice_max(a: &VecDeque<f32>, b: &VecDeque<f32>) -> f32 {
    let peak = a.iter().chain(b.iter()).copied().fold(0.0_f32, f32::max);
    if peak <= 0.0 {
        return 1024.0;
    }
    let base = 10f32.powf(peak.log10().floor());
    let m = peak / base;
    let step = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    (step * base).max(1024.0)
}

/// 既に確保済みの矩形内にミニグラフを描く (左ナビ項目・コア別グラフ用)。
pub fn paint_mini_graph(
    painter: &egui::Painter,
    rect: egui::Rect,
    values: &VecDeque<f32>,
    max: f32,
    color: Color32,
    style: GraphStyle,
) {
    paint_curve(painter, rect, values, max, color, 1.2, style);
}

/// 大きな使用率グラフ (パフォーマンスタブ用)。25/50/75% のグリッド線付き。
/// 右クリックメニュー用にクリック可能な応答を返す。
pub fn perf_chart(
    ui: &mut egui::Ui,
    values: &VecDeque<f32>,
    max: f32,
    color: Color32,
    size: Vec2,
    style: GraphStyle,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
        // グリッド線
        let grid = Color32::from_gray(45);
        for f in [0.25, 0.5, 0.75] {
            let y = rect.bottom() - 2.0 - f * (rect.height() - 4.0);
            painter.line_segment(
                [Pos2::new(rect.left() + 2.0, y), Pos2::new(rect.right() - 2.0, y)],
                egui::Stroke::new(1.0, grid),
            );
        }
        paint_curve(painter, rect, values, max, color, 2.0, style);
        // 本家と同じスケール表示 (グラフ右上)
        painter.text(
            egui::pos2(rect.right() - 5.0, rect.top() + 2.0),
            egui::Align2::RIGHT_TOP,
            format!("{max:.0}%"),
            egui::FontId::proportional(11.0),
            Color32::from_gray(150),
        );
        // 本家と同じ時間目盛り (左 = 30 秒前、右 = 現在)
        let stamp = Color32::from_gray(110);
        let font = egui::FontId::proportional(10.0);
        painter.text(
            egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            "30 秒",
            font.clone(),
            stamp,
        );
        painter.text(
            egui::pos2(rect.right() - 4.0, rect.bottom() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            "0",
            font,
            stamp,
        );
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
