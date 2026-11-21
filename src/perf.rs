//! NPU 使用率計測 (PDH パフォーマンスカウンタ)。
//!
//! Copilot+ PC 等の NPU 搭載機では `\NPU Engine(*)\Utilization Percentage`
//! カウンタが公開される。非搭載機ではカウンタ登録に失敗し、
//! `available = false` として扱う (NPU グラフは 0% 表示)。

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

const NPU_COUNTER: &str = r"\NPU Engine(*)\Utilization Percentage";

pub struct NpuMeter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// カウンタが利用可能か (NPU 非搭載機では false)
    pub available: bool,
}

impl NpuMeter {
    pub fn new() -> Self {
        unsafe {
            let mut query: PDH_HQUERY = std::ptr::null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != ERROR_SUCCESS {
                return Self {
                    query: std::ptr::null_mut(),
                    counter: std::ptr::null_mut(),
                    available: false,
                };
            }
            let path: Vec<u16> = NPU_COUNTER.encode_utf16().chain(std::iter::once(0)).collect();
            let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != ERROR_SUCCESS {
                PdhCloseQuery(query);
                return Self {
                    query: std::ptr::null_mut(),
                    counter: std::ptr::null_mut(),
                    available: false,
                };
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

    /// 全 NPU エンジンの使用率合計 (0..100 に丸め)。取得不可なら None。
    pub fn usage(&mut self) -> Option<f32> {
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
                return Some(0.0);
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
            let total: f64 = items
                .iter()
                .map(|item| item.FmtValue.Anonymous.doubleValue)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .sum();
            Some((total as f32).clamp(0.0, 100.0))
        }
    }
}

impl Drop for NpuMeter {
    fn drop(&mut self) {
        if !self.query.is_null() {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}
