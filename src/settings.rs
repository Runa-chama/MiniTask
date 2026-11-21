//! 設定の永続化 (HKCU\Software\MiniTask)。
//! 更新の速度・グラフ表示・列表示・ソート・最後に開いたタブ等を保存する。

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

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
        }
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
            if [500, 1000, 5000].contains(&v) {
                s.interval_ms = v;
            }
        }
        if let Ok(v) = key.get_value::<u32, _>("GraphAnimation") {
            s.graph_animation = v != 0;
        }
        if let Ok(v) = key.get_value::<u32, _>("GraphSpline") {
            s.graph_spline = v != 0;
        }
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
    }

    /// 現在値から変更されたかを判定するための比較用スナップショット。
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
    }
}
