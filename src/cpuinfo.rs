//! CPU の静的情報 (基本クロック / 仮想化 / キャッシュ容量) を取得する。

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::System::SystemInformation::{
    CacheData, CacheInstruction, CACHE_RELATIONSHIP, GetLogicalProcessorInformationEx,
    RelationCache, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

#[derive(Clone, Debug, Default)]
pub struct CpuInfo {
    /// 基本クロック (MHz)
    pub base_mhz: Option<u32>,
    /// 物理コア数
    pub physical_cores: Option<u32>,
    /// ハードウェア仮想化が有効か
    pub virtualization: Option<bool>,
    /// (ラベル, 合計バイト数)
    pub caches: Vec<(String, u64)>,
}

#[allow(non_upper_case_globals)]
pub fn query() -> CpuInfo {
    let mut info = CpuInfo::default();

    // レジストリから基本クロックを取得
    if let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
    {
        info.base_mhz = key.get_value::<u32, _>("~MHz").ok();
    }

    unsafe {
        info.virtualization = Some(IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) != 0);

        // まず必要なバッファ長を問い合わせる
        let mut size = 0u32;
        let _ = GetLogicalProcessorInformationEx(RelationCache, std::ptr::null_mut(), &mut size);
        if size == 0 {
            return info;
        }
        let mut buf = vec![0u8; size as usize];
        if GetLogicalProcessorInformationEx(
            RelationCache,
            buf.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            &mut size,
        ) == 0
        {
            let err = std::io::Error::last_os_error().raw_os_error();
            if err != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                return info;
            }
            if size as usize > buf.len() {
                buf.resize(size as usize, 0);
                if GetLogicalProcessorInformationEx(
                    RelationCache,
                    buf.as_mut_ptr()
                        .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                    &mut size,
                ) == 0
                {
                    return info;
                }
            }
        }

        let record_size = std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>();
        let mut offset = 0usize;
        while offset + record_size <= buf.len() {
            let rec = std::ptr::read_unaligned(
                buf[offset..].as_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            );
            if rec.Size == 0 {
                break;
            }
            let advance = rec.Size as usize;
            if advance < record_size || offset + advance > buf.len() {
                break;
            }
            if rec.Relationship == RelationCache {
                let cache: CACHE_RELATIONSHIP = rec.Anonymous.Cache;
                let label = match (cache.Level, cache.Type) {
                    (1, CacheData) => "L1 データ".to_owned(),
                    (1, CacheInstruction) => "L1 命令".to_owned(),
                    (1, _) => "L1".to_owned(),
                    (2, _) => "L2".to_owned(),
                    (3, _) => "L3".to_owned(),
                    (level, _) => format!("L{level}"),
                };
                if let Some(entry) = info.caches.iter_mut().find(|(name, _)| name == &label) {
                    entry.1 += cache.CacheSize as u64;
                } else {
                    info.caches.push((label, cache.CacheSize as u64));
                }
            }
            offset += advance;
        }
        info.caches.sort_by(|a, b| a.0.cmp(&b.0));
    }
    info
}
