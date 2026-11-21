//! ディスクの SMART / NVMe ヘルス情報を取得する。
//!
//! \\\\.\\PhysicalDriveN に対して複数の IOCTL を順に試し、
//! 最初に成功した経路の情報を返す (NVMe → ATA SMART → SAT → PREDICT_FAILURE)。

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, BusTypeAta, BusTypeNvme, BusTypeSata, BusTypeUsb, FILE_ATTRIBUTE_NORMAL,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_PREDICT_FAILURE, IOCTL_STORAGE_QUERY_PROPERTY, NVMeDataTypeLogPage,
    PropertyStandardQuery, ProtocolTypeNvme, SENDCMDINPARAMS, SENDCMDOUTPARAMS, SMART_RCV_DRIVE_DATA,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PREDICT_FAILURE, STORAGE_PROTOCOL_DATA_DESCRIPTOR,
    STORAGE_PROTOCOL_SPECIFIC_DATA, STORAGE_PROPERTY_QUERY, StorageAdapterProtocolSpecificProperty,
    StorageDeviceProtocolSpecificProperty, StorageDeviceProperty, StorageDeviceTemperatureProperty,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SmartStatus {
    Good,
    Caution,
    Bad,
    #[default]
    Unknown,
}

impl SmartStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Good => "正常",
            Self::Caution => "注意",
            Self::Bad => "異常",
            Self::Unknown => "取得不可",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SmartAttribute {
    pub id: u8,
    pub name: String,
    pub current: u8,
    pub worst: u8,
    pub threshold: u8,
    pub raw: u64,
    pub raw_text: String,
}

#[derive(Clone, Debug, Default)]
pub struct SmartInfo {
    pub available: bool,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub interface: String,
    pub health_pct: Option<u8>,
    pub temperature_c: Option<i16>,
    pub status: SmartStatus,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    pub total_reads_gb: Option<f64>,
    pub total_writes_gb: Option<f64>,
    pub attributes: Vec<SmartAttribute>,
    pub extra: Vec<(String, String)>,
}

impl SmartInfo {
    pub fn status_text(&self) -> String {
        match (self.status, self.health_pct) {
            (SmartStatus::Unknown, _) => "取得不可".to_owned(),
            (s, Some(p)) => format!("{} ({p}%)", s.label()),
            (s, None) => s.label().to_owned(),
        }
    }
}

/// NVMe SMART / Health Information Log Page (512 bytes、指定されたレイアウト)。
#[repr(C)]
#[derive(Clone, Copy)]
struct NvmeHealthLog {
    critical_warning: u8,
    temperature: [u8; 2],
    available_spare: u8,
    available_spare_threshold: u8,
    percentage_used: u8,
    _reserved1: [u8; 26],
    data_units_read: [u8; 16],
    data_units_written: [u8; 16],
    host_reads: [u8; 16],
    host_writes: [u8; 16],
    controller_busy_time: [u8; 16],
    power_cycles: [u8; 16],
    power_on_hours: [u8; 16],
    unsafe_shutdowns: [u8; 16],
    media_errors: [u8; 16],
    num_err_info_log_entries: [u8; 16],
    warning_temp_time: u32,
    critical_temp_time: u32,
    temp_sensor: [u16; 8],
}

fn le_u64(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, x) in b.iter().take(8).enumerate() {
        v |= (*x as u64) << (8 * i);
    }
    v
}

fn smart_attr_name(id: u8) -> &'static str {
    match id {
        0x01 => "読み取りエラー率",
        0x02 => "スループット性能",
        0x03 => "スピンアップ時間",
        0x04 => "スピンアップ回数",
        0x05 => "代替処理済のセクタ数",
        0x06 => "シークエラー率",
        0x07 => "シーク性能",
        0x08 => "シーク時間",
        0x09 => "電源投入時間",
        0x0A => "スピンアップ再試行回数",
        0x0B => "キャリブレーション再試行回数",
        0x0C => "電源投入回数",
        0x0D => "ソフトウェア読み取りエラー率",
        0xAE => "Unexpected Loss of Power",
        0xB1 => "ウェアレベリング回数",
        0xB3 => "使用済みブロック数",
        0xB4 => "未使用ブロック数",
        0xB5 => "プログラム失敗回数",
        0xB6 => "消去失敗回数",
        0xB7 => "不良ブロック数",
        0xB8 => "EOL/媒体摩耗指標",
        0xBB => "Uncorrectable Error Count",
        0xBE => "エアフロー温度",
        0xC2 => "温度",
        0xC3 => "ハードウェア ECC 回数",
        0xC4 => "再割り当てイベント数",
        0xC5 => "代替処理保留中のセクタ数",
        0xC6 => "回復不可能セクタ数",
        0xC7 => "インターフェース エラー",
        0xD1 => "SSD 摩耗指標",
        0xE1 => "総書込量 (ホスト)",
        0xE2 => "総読込量 (ホスト)",
        0xE7 => "SSD 寿命",
        0xE9 => "総書込量 (媒体)",
        0xF1 => "総書込量 (SSD)",
        0xF2 => "総読込量 (SSD)",
        0xE8 => "利用可能な予備領域",
        0xEB => "予備領域",
        0xF0 => "未使用予備領域",
        _ => "属性",
    }
}

fn raw_text(id: u8, raw: u64) -> String {
    match id {
        0x09 | 0xB1 | 0xB3 | 0xB4 | 0xB7 => {
            let h = (raw >> 32) as u32 + (raw & 0xFFFFFFFF) as u32 * 2;
            format!("{h} 回")
        }
        0x0C => format!("{} 回", (raw >> 32) as u32 + (raw & 0xFFFFFFFF) as u32),
        0xBE | 0xC2 => format!("{} °C", raw & 0xFF),
        0xE1 | 0xE2 | 0xE9 | 0xF1 | 0xF2 => format!("{:.2} TB", raw as f64 * 512.0 / 1_000_000_000.0),
        0xC4 | 0xC5 | 0xC6 => format!("{} セクタ", raw & 0xFFFFFFFF),
        _ => format!("0x{raw:016X}"),
    }
}

/// ATA SMART READ DATA / READ THRESHOLDS の bBuffer (512 bytes) から属性を解析する。
pub fn parse_ata_attributes(data: &[u8], thresholds: Option<&[u8]>) -> Vec<SmartAttribute> {
    let mut out = Vec::new();
    if data.len() < 2 {
        return out;
    }
    for i in 0..30 {
        let off = 2 + i * 12;
        if off + 12 > data.len() {
            break;
        }
        let e = &data[off..off + 12];
        let id = e[0];
        if id == 0 {
            break;
        }
        let threshold = thresholds
            .and_then(|t| {
                let to = 2 + i * 12;
                t.get(to..to + 12).map(|x| x[1])
            })
            .unwrap_or(0);
        let raw = le_u64(&e[5..11]);
        out.push(SmartAttribute {
            id,
            name: smart_attr_name(id).to_owned(),
            current: e[3],
            worst: e[4],
            threshold,
            raw,
            raw_text: raw_text(id, raw),
        });
    }
    out
}

/// NVMe ヘルスログから温度・健康状態・GB 等を取り出す (純粋関数)。
pub fn parse_nvme_health(log: &[u8]) -> Option<(i16, u8, u8, u8, f64, f64)> {
    if log.len() < std::mem::size_of::<NvmeHealthLog>() {
        return None;
    }
    unsafe {
        let h = std::ptr::read_unaligned(log.as_ptr().cast::<NvmeHealthLog>());
        let temp_k = u16::from_le_bytes(h.temperature) as i32;
        let health = (100i32 - h.percentage_used as i32).clamp(0, 100) as u8;
        let read_gb = le_u64(&h.data_units_read) as f64 * 512.0 * 1000.0 / 1_000_000_000.0;
        let write_gb = le_u64(&h.data_units_written) as f64 * 512.0 * 1000.0 / 1_000_000_000.0;
        Some((
            (temp_k - 273).clamp(-273, 500) as i16,
            health,
            h.available_spare,
            h.available_spare_threshold,
            read_gb,
            write_gb,
        ))
    }
}

unsafe fn open_drive(disk_id: &str) -> Option<*mut std::ffi::c_void> { unsafe {
    let path: Vec<u16> = format!("\\\\.\\PhysicalDrive{}\\0", disk_id)
        .encode_utf16()
        .collect();
    let h = CreateFileW(
        path.as_ptr(),
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        std::ptr::null_mut(),
    );
    if h == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(h)
    }
}}

unsafe fn string_at(buffer: &[u8], offset: u32) -> String {
    if offset == 0 || offset as usize >= buffer.len() {
        return String::new();
    }
    let rest = &buffer[offset as usize..];
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).trim().to_owned()
}

unsafe fn device_info(handle: *mut std::ffi::c_void) -> (String, String, String, String) { unsafe {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buf = [0u8; 4096];
    let mut bytes = 0u32;
    if DeviceIoControl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &query as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
        buf.as_mut_ptr().cast(),
        buf.len() as u32,
        &mut bytes,
        std::ptr::null_mut(),
    ) == 0
        || (bytes as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>()
    {
        return Default::default();
    }
    let d = std::ptr::read_unaligned(buf.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>());
    let vendor = string_at(&buf, d.VendorIdOffset);
    let product = string_at(&buf, d.ProductIdOffset);
    let model = if vendor.is_empty() {
        product
    } else {
        format!("{vendor} {product}").trim().to_owned()
    };
    let iface = if d.BusType == BusTypeNvme {
        "NVMe"
    } else if d.BusType == BusTypeUsb {
        "USB"
    } else if d.BusType == BusTypeSata {
        "SATA"
    } else if d.BusType == BusTypeAta {
        "ATA"
    } else {
        "SCSI"
    }
    .to_owned();
    (
        model,
        string_at(&buf, d.SerialNumberOffset),
        string_at(&buf, d.ProductRevisionOffset),
        iface,
    )
}}

unsafe fn query_nvme(handle: *mut std::ffi::c_void, info: &mut SmartInfo) -> bool { unsafe {
    let spec = STORAGE_PROTOCOL_SPECIFIC_DATA {
        ProtocolType: ProtocolTypeNvme,
        DataType: NVMeDataTypeLogPage as u32,
        ProtocolDataRequestValue: 0x02, // SMART / Health Information Log Page
        ProtocolDataRequestSubValue: 0,
        ProtocolDataOffset: (std::mem::size_of::<STORAGE_PROPERTY_QUERY>()
            + std::mem::size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>()) as u32,
        ProtocolDataLength: 512,
        FixedProtocolReturnData: 0,
        ProtocolDataRequestSubValue2: 0,
        ProtocolDataRequestSubValue3: 0,
        ProtocolDataRequestSubValue4: 0,
    };
    let mut in_buf = [0u8; 1024];
    let q = in_buf.as_mut_ptr().cast::<STORAGE_PROPERTY_QUERY>();
    *q = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProtocolSpecificProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let s = in_buf
        .as_mut_ptr()
        .add(std::mem::size_of::<STORAGE_PROPERTY_QUERY>())
        .cast::<STORAGE_PROTOCOL_SPECIFIC_DATA>();
    *s = spec;
    let mut out = [0u8; 2048];
    let mut bytes = 0u32;
    let ok = DeviceIoControl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        in_buf.as_ptr().cast(),
        (std::mem::size_of::<STORAGE_PROPERTY_QUERY>()
            + std::mem::size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>()) as u32,
        out.as_mut_ptr().cast(),
        out.len() as u32,
        &mut bytes,
        std::ptr::null_mut(),
    );
    if ok == 0 {
        // アダプター要求に切り替えて再試行
        (*q).PropertyId = StorageAdapterProtocolSpecificProperty;
        bytes = 0;
        if DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            in_buf.as_ptr().cast(),
            (std::mem::size_of::<STORAGE_PROPERTY_QUERY>()
                + std::mem::size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>()) as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut bytes,
            std::ptr::null_mut(),
        ) == 0
        {
            return false;
        }
    }
    if (bytes as usize) < std::mem::size_of::<STORAGE_PROTOCOL_DATA_DESCRIPTOR>() {
        return false;
    }
    let desc = std::ptr::read_unaligned(out.as_ptr().cast::<STORAGE_PROTOCOL_DATA_DESCRIPTOR>());
    let off = desc.ProtocolSpecificData.ProtocolDataOffset as usize;
    let len = desc.ProtocolSpecificData.ProtocolDataLength as usize;
    if off == 0 || off + len > out.len() || len < std::mem::size_of::<NvmeHealthLog>() {
        return false;
    }
    let Some((temp, health, spare, spare_th, read_gb, write_gb)) =
        parse_nvme_health(&out[off..off + len])
    else {
        return false;
    };
    let log = &out[off..off + len];
    let h = std::ptr::read_unaligned(log.as_ptr().cast::<NvmeHealthLog>());
    info.available = true;
    info.temperature_c = Some(temp);
    info.health_pct = Some(health);
    info.total_reads_gb = Some(read_gb);
    info.total_writes_gb = Some(write_gb);
    info.power_cycles = Some(le_u64(&h.power_cycles));
    info.power_on_hours = Some(le_u64(&h.power_on_hours));
    info.status = if h.critical_warning != 0 || spare < spare_th {
        SmartStatus::Bad
    } else if health <= 10 || spare <= spare_th {
        SmartStatus::Caution
    } else {
        SmartStatus::Good
    };
    let values = [
        ("予備領域", format!("{spare}% (しきい値 {spare_th}%)")),
        ("総読込量", format!("{read_gb:.2} GB")),
        ("総書込量", format!("{write_gb:.2} GB")),
        ("電源投入回数", le_u64(&h.power_cycles).to_string()),
        ("使用時間", format!("{} 時間", le_u64(&h.power_on_hours))),
        ("緊急シャットダウン", le_u64(&h.unsafe_shutdowns).to_string()),
        ("メディア エラー", le_u64(&h.media_errors).to_string()),
        ("エラー ログ エントリ", le_u64(&h.num_err_info_log_entries).to_string()),
        ("警告温度時間", format!("{} 分", h.warning_temp_time)),
        ("危険温度時間", format!("{} 分", h.critical_temp_time)),
    ];
    info.extra.extend(values.into_iter().map(|(k, v)| (k.to_owned(), v)));
    true
}}

unsafe fn smart_read(handle: *mut std::ffi::c_void, feature: u8, cyl_low: u8, cyl_high: u8) -> Option<[u8; 512]> { unsafe {
    let input = SENDCMDINPARAMS {
        cBufferSize: 512,
        irDriveRegs: windows_sys::Win32::System::Ioctl::IDEREGS {
            bFeaturesReg: feature,
            bSectorCountReg: 0,
            bSectorNumberReg: 0,
            bCylLowReg: cyl_low,
            bCylHighReg: cyl_high,
            bDriveHeadReg: 0xA0,
            bCommandReg: 0xB0, // SMART_CMD
            bReserved: 0,
        },
        bDriveNumber: 0,
        bReserved: [0; 3],
        dwReserved: [0; 4],
        bBuffer: [0],
    };
    let mut output = [0u8; std::mem::size_of::<SENDCMDOUTPARAMS>() + 512];
    let mut bytes = 0u32;
    let ok = DeviceIoControl(
        handle,
        SMART_RCV_DRIVE_DATA,
        &input as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<SENDCMDINPARAMS>() as u32,
        output.as_mut_ptr().cast(),
        output.len() as u32,
        &mut bytes,
        std::ptr::null_mut(),
    );
    if ok == 0 || (bytes as usize) < std::mem::size_of::<SENDCMDOUTPARAMS>() + 510 {
        return None;
    }
    let mut data = [0u8; 512];
    let start = std::mem::size_of::<SENDCMDOUTPARAMS>() - 1;
    data.copy_from_slice(&output[start..start + 512]);
    Some(data)
}}

unsafe fn query_ata(handle: *mut std::ffi::c_void, info: &mut SmartInfo) -> bool { unsafe {
    let Some(data) = smart_read(handle, 0xD0, 0x4F, 0xC2) else {
        return false;
    };
    let thresholds = smart_read(handle, 0xD1, 0x4F, 0xC2);
    let attrs = parse_ata_attributes(&data, thresholds.as_ref().map(|v| v.as_slice()));
    if attrs.is_empty() {
        return false;
    }
    let temp = attrs
        .iter()
        .find(|a| a.id == 0xC2 || a.id == 0xBE)
        .map(|a| (a.raw & 0xFF) as i16);
    let mut bad = false;
    let mut caution = false;
    let mut health = None;
    for a in &attrs {
        if a.threshold > 0 && a.current <= a.threshold {
            bad = true;
        }
        if matches!(a.id, 0xC5 | 0xC6) && a.raw > 0 {
            caution = true;
        }
        if matches!(a.id, 0xE7 | 0xEB | 0xF0 | 0xB1) && a.current > 0 && a.current <= 100 {
            health = Some(health.map_or(a.current, |h: u8| h.min(a.current)));
        }
    }
    info.available = true;
    info.temperature_c = temp;
    info.health_pct = health.or(if bad { None } else { Some(100) });
    info.status = if bad {
        SmartStatus::Bad
    } else if caution || health.is_some_and(|h| h <= 20) {
        SmartStatus::Caution
    } else {
        SmartStatus::Good
    };
    info.power_on_hours = attrs.iter().find(|a| a.id == 0x09).map(|a| (a.raw >> 32) as u64);
    info.power_cycles = attrs.iter().find(|a| a.id == 0x0C).map(|a| a.raw);
    info.attributes = attrs;
    true
}}

unsafe fn query_temperature(handle: *mut std::ffi::c_void, info: &mut SmartInfo) { unsafe {
    if info.temperature_c.is_some() {
        return;
    }
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceTemperatureProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buf = [0u8; 256];
    let mut bytes = 0u32;
    if DeviceIoControl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &query as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
        buf.as_mut_ptr().cast(),
        buf.len() as u32,
        &mut bytes,
        std::ptr::null_mut(),
    ) != 0
        && bytes >= 8
    {
        let kelvin = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as i32;
        if (200..=500).contains(&kelvin) {
            info.temperature_c = Some((kelvin - 273) as i16);
        }
    }
}}

unsafe fn query_predict(handle: *mut std::ffi::c_void, info: &mut SmartInfo) -> bool { unsafe {
    let mut p = STORAGE_PREDICT_FAILURE::default();
    let mut bytes = 0u32;
    if DeviceIoControl(
        handle,
        IOCTL_STORAGE_PREDICT_FAILURE,
        std::ptr::null(),
        0,
        &mut p as *mut _ as *mut std::ffi::c_void,
        std::mem::size_of::<STORAGE_PREDICT_FAILURE>() as u32,
        &mut bytes,
        std::ptr::null_mut(),
    ) == 0
    {
        return false;
    }
    info.available = true;
    info.status = if p.PredictFailure != 0 {
        SmartStatus::Bad
    } else {
        SmartStatus::Good
    };
    true
}}

/// 物理ディスク番号 (PDH インスタンスの先頭数字) から SMART 情報を取得する。
pub fn query(disk_id: &str) -> SmartInfo {
    let mut info = SmartInfo {
        interface: "—".to_owned(),
        ..Default::default()
    };
    if disk_id.is_empty() || !disk_id.bytes().all(|b| b.is_ascii_digit()) {
        return info;
    }
    unsafe {
        let Some(handle) = open_drive(disk_id) else {
            return info;
        };
        let (model, serial, firmware, iface) = device_info(handle);
        info.model = model;
        info.serial = serial;
        info.firmware = firmware;
        info.interface = iface.clone();
        let ok = if iface == "NVMe" {
            query_nvme(handle, &mut info)
        } else {
            false
        };
        if !ok {
            // ATA / SATA / USB (SAT) は SMART_RCV_DRIVE_DATA を試す
            let ata = query_ata(handle, &mut info);
            if !ata && !query_predict(handle, &mut info) {
                info.available = false;
                info.status = SmartStatus::Unknown;
            }
        }
        query_temperature(handle, &mut info);
        CloseHandle(handle);
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ata_attribute_parsing() {
        let mut data = [0u8; 512];
        // 属性 C2 (温度): 現在値 40, 最悪 35, 生の値 40
        data[2] = 0xC2;
        data[5] = 40; // 現在値
        data[6] = 35; // 最悪値
        data[7] = 40; // 生の値 (温度)
        // 属性 05 (代替処理済): 生の値 3
        data[14] = 0x05;
        data[19] = 3;
        // 属性 C5 (保留中): 生の値 0
        data[26] = 0xC5;
        let attrs = parse_ata_attributes(&data, None);
        assert_eq!(attrs.len(), 3);
        assert_eq!(attrs[0].id, 0xC2);
        assert_eq!(attrs[0].name, "温度");
        assert_eq!(attrs[0].raw_text, "40 °C");
        assert_eq!(attrs[1].raw_text, "0x0000000000000003");
    }

    #[test]
    fn nvme_log_parsing() {
        let mut log = [0u8; 512];
        log[0] = 0; // critical warning
        log[1..3].copy_from_slice(&(313u16).to_le_bytes()); // 40°C
        log[3] = 100; // available spare
        log[4] = 10; // threshold
        log[5] = 3; // percentage used => 97%
        log[32..40].copy_from_slice(&2u64.to_le_bytes()); // data units read
        let (temp, health, spare, threshold, read_gb, _) = parse_nvme_health(&log).unwrap();
        assert_eq!(temp, 40);
        assert_eq!(health, 97);
        assert_eq!((spare, threshold), (100, 10));
        assert!(read_gb > 0.0);
    }
}
