//! 設定の永続化 (HKCU\Software\MiniTask)。
//! 更新速度・グラフ・配色・列・ソート・タブ・システム統合を保存する。

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

use crate::util::Palette;

const SETTINGS_PATH: &str = r"Software\MiniTask";

/// 保存する設定値。
#[derive(Clone, Debug)]
pub struct AppSettings {
    /// 更新間隔 (ミリ秒): 500 / 1000 / 5000
    pub interval_ms: u32,
    pub graph_animation: bool,
    pub graph_spline: bool,
    /// 列表示のビットマスク (ColumnKind::ALL の順)
    pub columns_mask: u32,
    /// ソート列 (ColumnKind::ALL のインデックス)
    pub sort_col: u8,
    pub sort_desc: bool,
    /// 最後に開いたタブ (0=プロセス, 1=パフォーマンス, 2=スタートアップ)
    pub tab: u8,
    /// パフォーマンスタブの選択種別 (0=CPU, 1=メモリ, 2=ディスク, 3=GPU, 4=NPU, 5=ネットワーク)
    pub perf_kind: u8,
    /// 選択中のデバイス ID (空なら先頭デバイス)
    pub perf_id: String,
    /// CPU グラフモード (false=全体, true=論理プロセッサ)
    pub cpu_graph_mode: bool,

    // --- 外観・動作 ---
    pub dark_mode: bool,
    pub dark_colors: Palette,
    pub light_colors: Palette,
    pub always_on_top: bool,
    /// 0=前回のタブを復元, 1=常にプロセス, 2=常にパフォーマンス
    pub startup_tab_mode: u8,
    pub confirm_kill: bool,

    // --- システム統合 ---
    /// Ctrl+Shift+Esc の起動置換を有効にする
    pub replace_taskmgr: bool,
    /// タスクトレイにアイコンを常駐させる
    pub tray_icon: bool,
    /// 閉じるボタンで終了せずトレイに格納する
    pub close_to_tray: bool,
    /// 最小化ボタンでもトレイに格納する
    pub minimize_to_tray: bool,
    /// トレイアイコンのツールチップに CPU / メモリ使用率を表示する
    pub tray_usage_tip: bool,
    /// Windows サインイン時に MiniTask を自動起動する
    pub autostart: bool,
    /// 起動時にタスクトレイへ格納する
    pub start_in_tray: bool,
    /// 温度の単位 (0=摂氏, 1=華氏)
    pub temp_unit: u8,
    /// 使用するフォント (空=自動)
    pub font_family: String,
    /// 背景画像のファイルパス (空=無効)
    pub bg_image_path: String,
    /// 背景画像の表示方法 (0=引き伸ばす, 1=切り抜く, 2=全体を収める, 3=タイル)
    pub bg_fit: u8,
    /// 背景画像の上に乗せる背景色の不透明度 (0〜100)
    pub bg_overlay_alpha: u8,
    /// ウィンドウ全体の不透明度 (0〜100)
    pub window_opacity: u8,
    /// 最後に GitHub Releases をチェックした時刻 (Unix 秒)
    pub last_update_check: u64,
}

impl Default for AppSettings {
    fn default() -> Self {
        // 既定の表示列: 名前/状態/CPU/メモリ/ネット受信/ネット送信/GPU
        let default_columns = [
            true, true, false, true, true, true, true, false, false, true, false, false, false,
            false,
        ];
        let mut mask = 0u32;
        for (i, on) in default_columns.iter().enumerate() {
            if *on {
                mask |= 1 << i;
            }
        }
        Self {
            interval_ms: 1000,
            graph_animation: false,
            graph_spline: false,
            columns_mask: mask,
            sort_col: 3, // CPU
            sort_desc: true,
            tab: 0,
            perf_kind: 0,
            perf_id: String::new(),
            cpu_graph_mode: false,
            dark_mode: true,
            dark_colors: Palette::dark_default(),
            light_colors: Palette::light_default(),
            always_on_top: false,
            startup_tab_mode: 0,
            confirm_kill: true,
            replace_taskmgr: false,
            tray_icon: true,
            close_to_tray: true,
            minimize_to_tray: false,
            tray_usage_tip: true,
            autostart: false,
            start_in_tray: false,
            temp_unit: 0,
            font_family: "Meiryo".to_owned(),
            bg_image_path: String::new(),
            bg_fit: 0,
            bg_overlay_alpha: 25,
            window_opacity: 100,
            last_update_check: 0,
        }
    }
}

fn read_bool(key: &RegKey, name: &str, default: bool) -> bool {
    match key.get_value::<u32, _>(name) {
        Ok(v) => v != 0,
        Err(_) => default,
    }
}

fn read_palette(key: &RegKey, prefix: &str, default: Palette) -> Palette {
    let mut p = default;
    for (i, (k, name)) in Palette::keys().iter().zip(Palette::names().iter()).enumerate() {
        let full = format!("{prefix}{k}");
        let _ = name;
        if let Ok(v) = key.get_value::<String, _>(&full) {
            if let Some(c) = Palette::parse_hex(&v) {
                p.set(i, c);
            }
        }
    }
    p
}

fn write_palette(key: &RegKey, prefix: &str, palette: &Palette) {
    for (k, c) in Palette::keys().iter().zip(palette.as_array().iter()) {
        let _ = key.set_value(&format!("{prefix}{k}"), &Palette::to_hex(*c));
    }
}

impl AppSettings {
    /// レジストリから読み込む。値が無い/壊れていれば既定値を使う。
    pub fn load() -> Self {
        let mut s = Self::default();
        let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(SETTINGS_PATH, KEY_READ)
        else {
            return s;
        };
        if let Ok(v) = key.get_value::<u32, _>("IntervalMs") {
            if [500, 1000, 2000, 5000].contains(&v) {
                s.interval_ms = v;
            }
        }
        s.graph_animation = read_bool(&key, "GraphAnimation", s.graph_animation);
        s.graph_spline = read_bool(&key, "GraphSpline", s.graph_spline);
        if let Ok(v) = key.get_value::<u32, _>("ColumnsMask") {
            s.columns_mask = v;
        }
        if let Ok(v) = key.get_value::<u32, _>("SortCol") {
            if v < 14 {
                s.sort_col = v as u8;
            }
        }
        if let Ok(v) = key.get_value::<u32, _>("SortDesc") {
            s.sort_desc = v != 0;
        }
        if let Ok(v) = key.get_value::<u32, _>("Tab") {
            if v <= 2 {
                s.tab = v as u8;
            }
        }
        if let Ok(v) = key.get_value::<u32, _>("PerfKind") {
            if v <= 5 {
                s.perf_kind = v as u8;
            }
        }
        if let Ok(v) = key.get_value::<String, _>("PerfId") {
            s.perf_id = v;
        }
        if let Ok(v) = key.get_value::<u32, _>("CpuGraphMode") {
            s.cpu_graph_mode = v != 0;
        }
        s.dark_mode = read_bool(&key, "DarkMode", true);
        s.dark_colors = read_palette(&key, "Dark", Palette::dark_default());
        s.light_colors = read_palette(&key, "Light", Palette::light_default());
        s.always_on_top = read_bool(&key, "AlwaysOnTop", false);
        if let Ok(v) = key.get_value::<u32, _>("StartupTabMode") {
            if v <= 2 {
                s.startup_tab_mode = v as u8;
            }
        }
        s.confirm_kill = read_bool(&key, "ConfirmKill", true);
        s.replace_taskmgr = read_bool(&key, "ReplaceTaskmgr", false);
        s.tray_icon = read_bool(&key, "TrayIcon", true);
        s.close_to_tray = read_bool(&key, "CloseToTray", true);
        s.minimize_to_tray = read_bool(&key, "MinimizeToTray", false);
        s.tray_usage_tip = read_bool(&key, "TrayUsageTip", true);
        s.autostart = read_bool(&key, "Autostart", false);
        s.start_in_tray = read_bool(&key, "StartInTray", false);
        if let Ok(v) = key.get_value::<u32, _>("TempUnit") {
            if v <= 1 {
                s.temp_unit = v as u8;
            }
        }
        s.font_family = key
            .get_value::<String, _>("FontFamily")
            .unwrap_or_else(|_| s.font_family);
        s.bg_image_path = key
            .get_value::<String, _>("BgImagePath")
            .unwrap_or_else(|_| s.bg_image_path);
        if let Ok(v) = key.get_value::<u32, _>("BgImageFit") {
            if v <= 3 {
                s.bg_fit = v as u8;
            }
        }
        if let Ok(v) = key.get_value::<u32, _>("BgOverlayAlpha") {
            s.bg_overlay_alpha = (v.min(100)) as u8;
        }
        if let Ok(v) = key.get_value::<u32, _>("WindowOpacity") {
            s.window_opacity = (v.min(100)) as u8;
        }
        if let Ok(v) = key.get_value::<u64, _>("LastUpdateCheck") {
            s.last_update_check = v;
        }
        s
    }

    /// レジストリへ保存する。
    pub fn save(&self) {
        let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey_with_flags(SETTINGS_PATH, KEY_WRITE)
        else {
            return;
        };
        let _ = key.set_value("IntervalMs", &self.interval_ms);
        let _ = key.set_value("GraphAnimation", &(self.graph_animation as u32));
        let _ = key.set_value("GraphSpline", &(self.graph_spline as u32));
        let _ = key.set_value("ColumnsMask", &self.columns_mask);
        let _ = key.set_value("SortCol", &(self.sort_col as u32));
        let _ = key.set_value("SortDesc", &(self.sort_desc as u32));
        let _ = key.set_value("Tab", &(self.tab as u32));
        let _ = key.set_value("PerfKind", &(self.perf_kind as u32));
        let _ = key.set_value("PerfId", &self.perf_id);
        let _ = key.set_value("CpuGraphMode", &(self.cpu_graph_mode as u32));
        let _ = key.set_value("DarkMode", &(self.dark_mode as u32));
        write_palette(&key, "Dark", &self.dark_colors);
        write_palette(&key, "Light", &self.light_colors);
        let _ = key.set_value("AlwaysOnTop", &(self.always_on_top as u32));
        let _ = key.set_value("StartupTabMode", &(self.startup_tab_mode as u32));
        let _ = key.set_value("ConfirmKill", &(self.confirm_kill as u32));
        let _ = key.set_value("ReplaceTaskmgr", &(self.replace_taskmgr as u32));
        let _ = key.set_value("TrayIcon", &(self.tray_icon as u32));
        let _ = key.set_value("CloseToTray", &(self.close_to_tray as u32));
        let _ = key.set_value("MinimizeToTray", &(self.minimize_to_tray as u32));
        let _ = key.set_value("TrayUsageTip", &(self.tray_usage_tip as u32));
        let _ = key.set_value("Autostart", &(self.autostart as u32));
        let _ = key.set_value("StartInTray", &(self.start_in_tray as u32));
        let _ = key.set_value("TempUnit", &(self.temp_unit as u32));
        let _ = key.set_value("FontFamily", &self.font_family);
        let _ = key.set_value("BgImagePath", &self.bg_image_path);
        let _ = key.set_value("BgImageFit", &(self.bg_fit as u32));
        let _ = key.set_value("BgOverlayAlpha", &(self.bg_overlay_alpha as u32));
        let _ = key.set_value("WindowOpacity", &(self.window_opacity as u32));
        let _ = key.set_value("LastUpdateCheck", &self.last_update_check);
        // 旧設定・旧注入フックの残骸を掃除
        let _ = key.delete_value("TaskbarMenu");
        let _ = key.delete_value("HookInstalled");
        let _ = key.delete_value("HookFired");
        // 統合用: 自分のexeパスを書いておく
        if let Ok(exe) = std::env::current_exe() {
            let _ = key.set_value("ExePath", &exe.to_string_lossy().to_string());
        }
    }

    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    pub fn same(&self, other: &Self) -> bool {
        self.interval_ms == other.interval_ms
            && self.graph_animation == other.graph_animation
            && self.graph_spline == other.graph_spline
            && self.columns_mask == other.columns_mask
            && self.sort_col == other.sort_col
            && self.sort_desc == other.sort_desc
            && self.tab == other.tab
            && self.perf_kind == other.perf_kind
            && self.perf_id == other.perf_id
            && self.cpu_graph_mode == other.cpu_graph_mode
            && self.dark_mode == other.dark_mode
            && self.dark_colors == other.dark_colors
            && self.light_colors == other.light_colors
            && self.always_on_top == other.always_on_top
            && self.startup_tab_mode == other.startup_tab_mode
            && self.confirm_kill == other.confirm_kill
            && self.replace_taskmgr == other.replace_taskmgr
            && self.tray_icon == other.tray_icon
            && self.close_to_tray == other.close_to_tray
            && self.minimize_to_tray == other.minimize_to_tray
            && self.tray_usage_tip == other.tray_usage_tip
            && self.autostart == other.autostart
            && self.start_in_tray == other.start_in_tray
            && self.temp_unit == other.temp_unit
            && self.font_family == other.font_family
            && self.bg_image_path == other.bg_image_path
            && self.bg_fit == other.bg_fit
            && self.bg_overlay_alpha == other.bg_overlay_alpha
            && self.window_opacity == other.window_opacity
            && self.last_update_check == other.last_update_check
    }

    /// 現在のモードに対応するパレット。
    pub fn active_palette(&self) -> Palette {
        if self.dark_mode {
            self.dark_colors
        } else {
            self.light_colors
        }
    }

    /// 起動時に適用するタブ (設定で固定されている場合は上書きする)。
    pub fn initial_tab(&self) -> u8 {
        match self.startup_tab_mode {
            1 => 0,
            2 => 1,
            _ => self.tab,
        }
    }
}
