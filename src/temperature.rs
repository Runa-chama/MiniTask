//! CPU / GPU の温度を取得する。
//! CPU = PDH のサーマルゾーン、GPU = NVAPI / AMD ADL (動的ロード)。
#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_void, CStr};
use std::mem::size_of;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW, PDH_HCOUNTER,
    PDH_HQUERY,
};

use crate::perf::collect_counter_array;

/// PDH の温度カウンタを保持するプローブ。
pub struct CpuTempProbe {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl CpuTempProbe {
    pub fn new() -> Self {
        unsafe {
            let mut query: PDH_HQUERY = std::ptr::null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return Self {
                    query: std::ptr::null_mut(),
                    counter: std::ptr::null_mut(),
                };
            }
            let name: Vec<u16> = r"\Thermal Zone Information(*)\Temperature"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
            if PdhAddEnglishCounterW(query, name.as_ptr(), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return Self {
                    query: std::ptr::null_mut(),
                    counter: std::ptr::null_mut(),
                };
            }
            Self { query, counter }
        }
    }

    /// もっとも高いあり得る温度を摂氏で返す。取得不可なら None。
    pub fn read(&mut self) -> Option<f32> {
        if self.query.is_null() {
            return None;
        }
        unsafe {
            PdhCollectQueryData(self.query);
            let values = collect_counter_array(self.counter)?;
            let mut best: Option<f32> = None;
            for (_, raw) in values {
                for c in to_celsius(raw) {
                    if (-10.0..=120.0).contains(&c) && best.map_or(true, |b| c > b) {
                        best = Some(c);
                    }
                }
            }
            best
        }
    }
}

impl Drop for CpuTempProbe {
    fn drop(&mut self) {
        if !self.query.is_null() {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}

/// ファームウェアによって単位が異なるため、複数の解釈を試して妥当な値を選ぶ。
fn to_celsius(raw: f64) -> [f32; 4] {
    [
        (raw / 10.0 - 273.15) as f32, // ケルビン ×10
        (raw / 10.0) as f32,          // 摂氏 ×10
        (raw - 273.15) as f32,        // ケルビン
        raw as f32,                   // 摂氏
    ]
}

unsafe fn load(name: &str) -> Option<HMODULE> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
    if h.is_null() {
        None
    } else {
        Some(h)
    }
}

unsafe fn symbol<T: Copy>(module: HMODULE, name: &[u8]) -> Option<T> {
    let name = CStr::from_bytes_with_nul(name).ok()?;
    let p = unsafe { GetProcAddress(module, name.as_ptr() as *const u8) }?;
    Some(unsafe { std::mem::transmute_copy(&p) })
}

#[repr(C)]
struct NvThermalSensor {
    controller: u32,
    default_max_temp: u32,
    current_temp: i32,
}

#[repr(C)]
struct NvThermalSettings {
    version: u32,
    count: u32,
    sensors: [NvThermalSensor; 32],
}

/// NVIDIA の温度を取得 (NVAPI)。複数 GPU に対応。
unsafe fn nvapi_temps() -> Vec<f32> {
    let Some(module) = load("nvapi64.dll") else {
        return Vec::new();
    };
    let init: Option<unsafe extern "C" fn() -> i32> = symbol(module, b"NvAPI_Initialize\0");
    let enumeration: Option<unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32> =
        symbol(module, b"NvAPI_EnumPhysicalGPUs\0");
    let thermal: Option<unsafe extern "C" fn(*mut c_void, u32, *mut NvThermalSettings) -> i32> =
        symbol(module, b"NvAPI_GPU_GetThermalSettings\0");
    let (Some(init), Some(enumeration), Some(thermal)) = (init, enumeration, thermal) else {
        FreeLibrary(module);
        return Vec::new();
    };
    if init() != 0 {
        FreeLibrary(module);
        return Vec::new();
    }
    let mut gpus: [*mut c_void; 64] = [std::ptr::null_mut(); 64];
    let mut count = 0u32;
    let mut result = Vec::new();
    if enumeration(gpus.as_mut_ptr(), &mut count) == 0 {
        for gpu in gpus.iter().take(count.min(64) as usize) {
            if gpu.is_null() {
                continue;
            }
            let mut settings: NvThermalSettings = std::mem::zeroed();
            settings.version = (size_of::<NvThermalSettings>() as u32) | (2 << 16);
            if thermal(*gpu, 0, &mut settings) == 0 {
                if let Some(t) = (0..settings.count.min(32) as usize)
                    .map(|i| settings.sensors[i].current_temp as f32)
                    .find(|t| *t > 0.0 && *t < 130.0)
                {
                    result.push(t);
                }
            }
        }
    }
    FreeLibrary(module);
    result
}

#[repr(C)]
struct AdlTemperature {
    size: i32,
    temperature: i32, // 0.001 ℃
}

unsafe extern "C" fn adl_alloc(size: i32) -> *mut c_void {
    let layout = std::alloc::Layout::from_size_align(size.max(1) as usize, 8).unwrap();
    unsafe { std::alloc::alloc(layout) as *mut c_void }
}

/// AMD の温度を取得 (ADL)。複数 GPU に対応。
unsafe fn adl_temps() -> Vec<f32> {
    let Some(module) = load("atiadlxx.dll").or_else(|| load("atiadlxy.dll")) else {
        return Vec::new();
    };
    let create: Option<unsafe extern "C" fn(unsafe extern "C" fn(i32) -> *mut c_void, i32) -> i32> =
        symbol(module, b"ADL_Main_Control_Create\0");
    let count_fn: Option<unsafe extern "C" fn(*mut i32) -> i32> =
        symbol(module, b"ADL_Adapter_NumberOfAdapters_Get\0");
    let temp_fn: Option<unsafe extern "C" fn(i32, i32, *mut AdlTemperature) -> i32> =
        symbol(module, b"ADL_Overdrive5_Temperature_Get\0");
    let destroy: Option<unsafe extern "C" fn() -> i32> =
        symbol(module, b"ADL_Main_Control_Destroy\0");
    let (Some(create), Some(count_fn), Some(temp_fn), Some(destroy)) =
        (create, count_fn, temp_fn, destroy)
    else {
        FreeLibrary(module);
        return Vec::new();
    };
    if create(adl_alloc, 1) != 0 {
        FreeLibrary(module);
        return Vec::new();
    }
    let mut count = 0i32;
    let mut result = Vec::new();
    if count_fn(&mut count) == 0 {
        for index in 0..count {
            let mut temperature = AdlTemperature {
                size: size_of::<AdlTemperature>() as i32,
                temperature: 0,
            };
            if temp_fn(index, 0, &mut temperature) == 0 && temperature.temperature > 0 {
                let raw = temperature.temperature as f32;
                // ADL Overdrive5 は通常 1/1000 ℃。環境によっては 1/1 ℃ の場合もある。
                let c = if raw > 200.0 { raw / 1000.0 } else { raw };
                if (0.0..130.0).contains(&c) {
                    result.push(c);
                }
            }
        }
    }
    destroy();
    FreeLibrary(module);
    result
}

/// `nvidia-smi` から GPU 温度を取得するフォールバック。
fn nvidia_smi_temps() -> Vec<f32> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=temperature.gpu",
        "--format=csv,noheader,nounits",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<f32>().ok())
        .filter(|t| (0.0..130.0).contains(t))
        .collect()
}

/// GPU 温度を返す (検出順)。取得不可の要素は含めない。
pub fn gpu_temps() -> Vec<Option<f32>> {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<Option<f32>>)>> =
        std::sync::Mutex::new(None);

    if let Ok(guard) = CACHE.lock() {
        if let Some((at, ref v)) = *guard {
            if at.elapsed() < std::time::Duration::from_secs(2) {
                return v.clone();
            }
        }
    }

    let mut temps = unsafe { nvapi_temps() };
    if temps.is_empty() {
        temps = unsafe { adl_temps() };
    }
    if temps.is_empty() {
        temps = nvidia_smi_temps();
    }
    let out: Vec<Option<f32>> = temps.into_iter().map(Some).collect();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((std::time::Instant::now(), out.clone()));
    }
    out
}
