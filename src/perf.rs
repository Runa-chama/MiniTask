//! GPU / NPU エンジン使用率計測 (PDH パフォーマンスカウンタ)。
//!
//! タスクマネージャーと同じ仕組み:
//! `\GPU Engine(*)\Utilization Percentage` / `\NPU Engine(*)\Utilization Percentage`
//! の各カウンタ インスタンス名は
//! `pid_X_luid_..._eng_N_engtype_3d` の形式で、末尾の `engtype_X` が
//! エンジン種別 (3D / Copy / Video Decode など) を表す。
//! 種別ごとに Utilization Percentage を合計すると
//! 「項目ごとの使用率グラフ」が得られる (本家の「グラフの変更」の内部動作)。

use std::collections::{HashMap, HashSet};

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
    path: String,
    /// カウンタが利用可能か (NPU 非搭載機などでは false)
    pub available: bool,
}

impl EngineMeter {
    /// ワイルドカード付きカウンタパスでメータを作る。
    /// カウンタが存在しない場合は available = false になる。
    pub fn new(counter_path: &str) -> Self {
        let mut meter = Self {
            query: std::ptr::null_mut(),
            counter: std::ptr::null_mut(),
            path: counter_path.to_owned(),
            available: false,
        };
        meter.open();
        meter
    }

    /// カウンタを開く (利用可能なら true)。
    fn open(&mut self) -> bool {
        unsafe {
            let mut query: PDH_HQUERY = std::ptr::null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != ERROR_SUCCESS {
                return false;
            }
            let path: Vec<u16> = self.path.encode_utf16().chain(std::iter::once(0)).collect();
            let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != ERROR_SUCCESS {
                PdhCloseQuery(query);
                return false;
            }
            // 初期サンプル (最初の Collect は値が安定しない)
            PdhCollectQueryData(query);
            self.query = query;
            self.counter = counter;
            self.available = true;
            true
        }
    }

    /// カウンタが未開のとき再度開き直す (ドライバー更新等で後から現れる場合)。
    pub fn try_reopen(&mut self) -> bool {
        if self.available {
            return true;
        }
        self.open()
    }

    /// 全インスタンスを収集し (LUID, エンジン種別, 使用率) のリストを返す。
    /// Windows 11 では NPU も `\GPU Engine(*)` カウンタに含まれ、
    /// NPU アダプタ固有の LUID で列挙される (本家タスクマネージャーも
    /// この LUID で NPU と GPU を分けて表示している)。
    fn collect(&mut self) -> Option<Vec<(String, String, f64)>> {
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
                // インスタンス名:
                // pid_X_luid_0x00000000_0x0000f751_phys_0_eng_0_engtype_compute
                let name = wide_ptr_to_string(item.szName);
                let Some(engtype) = name.rsplit("_engtype_").next() else {
                    continue;
                };
                // luid_0xHHHHHHHH_0xLLLLLLLL を取り出す
                let luid = name
                    .split("_luid_")
                    .nth(1)
                    .and_then(|s| s.split("_phys_").next())
                    .unwrap_or("")
                    .to_owned();
                let value = item.FmtValue.Anonymous.doubleValue;
                if !value.is_finite() || value < 0.0 {
                    continue;
                }
                out.push((luid, engtype.to_lowercase(), value));
            }
            Some(out)
        }
    }

    /// 3D / Copy / 動画系エンジンを持つ LUID は GPU アダプタ。
    /// compute 系のみの LUID は NPU (Compute Accelerator) と判定する。
    fn is_gpu_engine(engtype: &str) -> bool {
        ["3d", "copy", "video", "display", "security", "timer", "jpeg", "codec"]
            .iter()
            .any(|key| engtype.contains(key))
    }

    fn sorted_usage(map: HashMap<String, f32>) -> Vec<(String, f32)> {
        let mut list: Vec<(String, f32)> = map.into_iter().collect();
        for (_, v) in list.iter_mut() {
            *v = v.clamp(0.0, 100.0);
        }
        list.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        list
    }

    /// 1 回の収集で GPU 用と NPU 用のエンジン別使用率を返す。
    /// `(GPU エンジン一覧, NPU エンジン一覧)`。
    pub fn read_split(&mut self) -> Option<(Vec<(String, f32)>, Vec<(String, f32)>)> {
        let samples = self.collect()?;
        let gpu_luids: HashSet<String> = samples
            .iter()
            .filter(|(_, engtype, _)| Self::is_gpu_engine(engtype))
            .map(|(luid, _, _)| luid.clone())
            .collect();
        let mut gpu: HashMap<String, f32> = HashMap::new();
        let mut npu: HashMap<String, f32> = HashMap::new();
        for (luid, engtype, value) in samples {
            let map = if gpu_luids.contains(&luid) {
                &mut gpu
            } else {
                &mut npu
            };
            *map.entry(engtype).or_insert(0.0) += value as f32;
        }
        Some((Self::sorted_usage(gpu), Self::sorted_usage(npu)))
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

/// NPU デバイス (ComputeAccelerator クラス) が搭載されているかどうか。
/// Ryzen AI / Snapdragon X / Intel NPU 等はこのデバイス クラスで列挙される。
/// カウンタが公開されていなくてもデバイス自体の有無はこれで判定できる。
pub fn npu_device_present() -> bool {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        DIGCF_PRESENT, SP_DEVINFO_DATA,
    };
    // ComputeAccelerator デバイス クラス {f01a9d53-3ff6-48d2-9f97-c8a7004be10c}
    const COMPUTE_ACCELERATOR: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(
        0xf01a9d53_3ff6_48d2_9f97_c8a7004be10c,
    );
    unsafe {
        let set = SetupDiGetClassDevsW(
            &COMPUTE_ACCELERATOR,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT,
        );
        // 失敗時は INVALID_HANDLE_VALUE (-1)
        if set == 0 || set == -1 {
            return false;
        }
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let present = SetupDiEnumDeviceInfo(set, 0, &mut data) != 0;
        SetupDiDestroyDeviceInfoList(set);
        present
    }
}

/// NPU のエンジン名 (本家タスクマネージャーは NPU の compute を
/// 「Compute」と表示する)。
pub fn npu_engine_display_name(engtype: &str) -> String {
    match engtype {
        "compute" => "Compute".to_owned(),
        other => engine_display_name(other),
    }
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
