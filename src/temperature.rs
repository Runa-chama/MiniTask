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

/// NVIDIA の温度を取得 (NVAPI)。
unsafe fn nvapi_temp() -> Option<f32> {
    let module = load("nvapi64.dll")?;
    let init: unsafe extern "C" fn() -> i32 = symbol(module, b"NvAPI_Initialize\0")?;
    let enumeration: unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32 =
        symbol(module, b"NvAPI_EnumPhysicalGPUs\0")?;
    let thermal: unsafe extern "C" fn(*mut c_void, u32, *mut NvThermalSettings) -> i32 =
        symbol(module, b"NvAPI_GPU_GetThermalSettings\0")?;
    if init() != 0 {
        FreeLibrary(module);
        return None;
    }
    let mut gpus: [*mut c_void; 64] = [std::ptr::null_mut(); 64];
    let mut count = 0u32;
    let result = if enumeration(gpus.as_mut_ptr(), &mut count) == 0 && count > 0 {
        let mut settings: NvThermalSettings = std::mem::zeroed();
        settings.version = (size_of::<NvThermalSettings>() as u32) | (2 << 16);
        if thermal(gpus[0], 0, &mut settings) == 0 {
            (0..settings.count.min(32) as usize)
                .map(|i| settings.sensors[i].current_temp as f32)
                .find(|t| *t > 0.0)
        } else {
            None
        }
    } else {
        None
    };
    FreeLibrary(module);
    result
}

#[repr(C)]
struct AdlTemperature {
    version: i32,
    value: i32,
}

unsafe extern "C" fn adl_alloc(size: i32) -> *mut c_void {
    let layout = std::alloc::Layout::from_size_align(size.max(1) as usize, 8).unwrap();
    unsafe { std::alloc::alloc(layout) as *mut c_void }
}

/// AMD の温度を取得 (ADL)。
unsafe fn adl_temp() -> Option<f32> {
    let module = load("atiadlxx.dll")?;
    let create: unsafe extern "C" fn(unsafe extern "C" fn(i32) -> *mut c_void, i32) -> i32 =
        symbol(module, b"ADL_Main_Control_Create\0")?;
    let count_fn: unsafe extern "C" fn(*mut i32) -> i32 =
        symbol(module, b"ADL_Adapter_NumberOfAdapters_Get\0")?;
    let temp_fn: unsafe extern "C" fn(i32, i32, *mut AdlTemperature) -> i32 =
        symbol(module, b"ADL_Overdrive5_Temperature_Get\0")?;
    let destroy: unsafe extern "C" fn() -> i32 = symbol(module, b"ADL_Main_Control_Destroy\0")?;
    if create(adl_alloc, 1) != 0 {
        FreeLibrary(module);
        return None;
    }
    let mut count = 0i32;
    let mut result = None;
    if count_fn(&mut count) == 0 && count > 0 {
        for index in 0..count {
            let mut temperature = AdlTemperature {
                version: size_of::<AdlTemperature>() as i32,
                value: 0,
            };
            if temp_fn(index, 0, &mut temperature) == 0 && temperature.value > 0 {
                // ADL は 0.1 度単位で返す
                result = Some(temperature.value as f32 / 10.0);
                break;
            }
        }
    }
    destroy();
    FreeLibrary(module);
    result
}

/// GPU 温度を返す (複数 GPU の場合は全件)。
pub fn gpu_temps() -> Vec<Option<f32>> {
    let vendor_temp = unsafe { nvapi_temp().or_else(|| adl_temp()) };
    vec![vendor_temp]
}
