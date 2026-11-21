//! GPU / NPU エンジン使用率計測 (PDH パフォーマンスカウンタ)。
//!
//! タスクマネージャーと同じ仕組み:
//! `\GPU Engine(*)\Utilization Percentage` / `\NPU Engine(*)\Utilization Percentage`
//! の各カウンタ インスタンス名は
//! `pid_X_luid_..._eng_N_engtype_3d` の形式で、末尾の `engtype_X` が
//! エンジン種別 (3D / Copy / Video Decode など) を表す。
//! 種別ごとに Utilization Percentage を合計すると
//! 「項目ごとの使用率グラフ」が得られる (本家の「グラフの変更」の内部動作)。

use std::collections::HashMap;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

/// NPU エンジンのカウンタパス。
pub const NPU_ENGINE_COUNTER: &str = r"\NPU Engine(*)\Utilization Percentage";
/// GPU エンジンのカウンタパス。
pub const GPU_ENGINE_COUNTER: &str = r"\GPU Engine(*)\Utilization Percentage";

/// エンジン種別ごとの使用率を集計する PDH メータ。
pub struct EngineMeter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// カウンタが利用可能か (NPU 非搭載機などでは false)
    pub available: bool,
}

impl EngineMeter {
    /// ワイルドカード付きカウンタパスでメータを作る。
    /// カウンタが存在しない場合は available = false になる。
    pub fn new(counter_path: &str) -> Self {
        unsafe {
            let mut query: PDH_HQUERY = std::ptr::null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != ERROR_SUCCESS {
                return Self::unavailable();
            }
            let path: Vec<u16> = counter_path.encode_utf16().chain(std::iter::once(0)).collect();
            let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != ERROR_SUCCESS {
                PdhCloseQuery(query);
                return Self::unavailable();
            }
            // 初期サンプル (最初の Collect は値が安定しない)
            PdhCollectQueryData(query);
            Self {
                query,
                counter,
                available: true,
            }
        }
    }

    fn unavailable() -> Self {
        Self {
            query: std::ptr::null_mut(),
            counter: std::ptr::null_mut(),
            available: false,
        }
    }

    /// 全インスタンスを収集し (エンジン種別, 使用率) のリストを返す。
    /// 種別はインスタンス名末尾の engtype_X (小文字化)。
    fn collect(&mut self) -> Option<Vec<(String, f64)>> {
        if !self.available {
            return None;
        }
        unsafe {
            PdhCollectQueryData(self.query);
            let mut size = 0u32;
            let mut count = 0u32;
            // 1 回目でバッファサイズと項目数を取得
            PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                std::ptr::null_mut(),
            );
            if size == 0 || count == 0 {
                return Some(Vec::new());
            }
            let mut buffer = vec![0u8; size as usize];
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                buffer.as_mut_ptr().cast(),
            );
            if status != ERROR_SUCCESS {
                return None;
            }
            let items = std::slice::from_raw_parts(
                buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                count as usize,
            );
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                // インスタンス名: pid_X_luid_..._eng_N_engtype_3d
                let name = wide_ptr_to_string(item.szName);
                let Some(engtype) = name.rsplit("_engtype_").next() else {
                    continue;
                };
                let value = item.FmtValue.Anonymous.doubleValue;
                if !value.is_finite() || value < 0.0 {
                    continue;
                }
                out.push((engtype.to_lowercase(), value));
            }
            Some(out)
        }
    }

    /// 全エンジンの合計使用率 (0..100 に丸め)。取得不可なら None。
    pub fn total_usage(&mut self) -> Option<f32> {
        let items = self.collect()?;
        let total: f64 = items.iter().map(|(_, v)| v).sum();
        Some((total as f32).clamp(0.0, 100.0))
    }

    /// エンジン種別ごとの使用率。使用率の降順にソート。
    pub fn usage_by_type(&mut self) -> Vec<(String, f32)> {
        let Some(items) = self.collect() else {
            return Vec::new();
        };
        let mut map: HashMap<String, f32> = HashMap::new();
        for (engtype, value) in items {
            *map.entry(engtype).or_insert(0.0) += value as f32;
        }
        let mut list: Vec<(String, f32)> = map.into_iter().collect();
        for (_, v) in list.iter_mut() {
            *v = v.clamp(0.0, 100.0);
        }
        list.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        list
    }
}

impl Drop for EngineMeter {
    fn drop(&mut self) {
        if !self.query.is_null() {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}

/// NUL 終端 UTF-16 ポインタを String に変換。
unsafe fn wide_ptr_to_string(p: windows_sys::core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
    }
    let slice = unsafe { std::slice::from_raw_parts(p, len) };
    String::from_utf16_lossy(slice)
}

/// エンジン種別の内部キー (英語小文字) を日本語表示名にする。
/// 本家タスクマネージャーの文言に準拠。末尾の " N" は番号として保持する。
pub fn engine_display_name(engtype: &str) -> String {
    // "compute 0" → base "compute" + 番号 0
    let (base, num) = match engtype.rsplit_once(' ') {
        Some((b, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
            (b, Some(n.to_owned()))
        }
        _ => (engtype, None),
    };
    let base_ja = match base {
        "3d" => "3D",
        "copy" => "コピー",
        "compute" => "コンピュート",
        "video decode" => "ビデオ デコード",
        "video encode" => "ビデオ エンコード",
        "video processing" => "ビデオ処理",
        "video codec" => "ビデオ コーデック",
        "video jpeg" => "ビデオ JPEG",
        "scene graph" => "シーングラフ",
        "machine learning" => "機械学習",
        "timer" => "タイマー",
        "security" => "セキュリティ",
        "high priority 3d" => "高優先度 3D",
        "high priority compute" => "高優先度コンピュート",
        other => other,
    };
    match num {
        Some(n) => format!("{base_ja} {n}"),
        None => base_ja.to_owned(),
    }
}
