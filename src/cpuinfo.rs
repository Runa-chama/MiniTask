//! CPU の静的情報 (基本クロック / コア数 / 仮想化 / キャッシュ容量) を取得する。

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::System::SystemInformation::{
    CacheData, CacheInstruction, GetLogicalProcessorInformationEx, RelationCache,
    RelationProcessorCore,
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

/// レコード列をバイト単位で辿る (構造体の Rust 側サイズと実レコード長が異なるため)。
fn records(buf: &[u8], mut visit: impl FnMut(u32, &[u8])) {
    let mut offset = 0usize;
    while offset + 8 <= buf.len() {
        let relationship = u32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap());
        let size = u32::from_ne_bytes(buf[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if size < 8 || offset + size > buf.len() {
            break;
        }
        visit(relationship, &buf[offset..offset + size]);
        offset += size;
    }
}

fn firmware_buffer(relationship: i32) -> Option<Vec<u8>> {
    unsafe {
        let mut size = 0u32;
        let _ = GetLogicalProcessorInformationEx(relationship, std::ptr::null_mut(), &mut size);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetLogicalProcessorInformationEx(relationship, buf.as_mut_ptr().cast(), &mut size) == 0 {
            let err = std::io::Error::last_os_error().raw_os_error();
            if err != Some(ERROR_INSUFFICIENT_BUFFER as i32) || size as usize > buf.len() {
                return None;
            }
            buf.resize(size as usize, 0);
            if GetLogicalProcessorInformationEx(relationship, buf.as_mut_ptr().cast(), &mut size)
                == 0
            {
                return None;
            }
        }
        Some(buf)
    }
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

    info.virtualization = Some(unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) } != 0);

    if let Some(buf) = firmware_buffer(RelationProcessorCore) {
        let mut count = 0u32;
        records(&buf, |_, _| count += 1);
        if count > 0 {
            info.physical_cores = Some(count);
        }
    }

    if let Some(buf) = firmware_buffer(RelationCache) {
        records(&buf, |relationship, record| {
            // CACHE_RELATIONSHIP: Level(+8), CacheSize(+12), Type(+16)
            if relationship as i32 != RelationCache || record.len() < 20 {
                return;
            }
            let level = record[8];
            let cache_size = u32::from_ne_bytes(record[12..16].try_into().unwrap()) as u64;
            let kind = u32::from_ne_bytes(record[16..20].try_into().unwrap()) as i32;
            let label = match (level, kind) {
                (1, CacheData) => "L1 データ".to_owned(),
                (1, CacheInstruction) => "L1 命令".to_owned(),
                (1, _) => "L1".to_owned(),
                (2, _) => "L2".to_owned(),
                (3, _) => "L3".to_owned(),
                (level, _) => format!("L{level}"),
            };
            if let Some(entry) = info.caches.iter_mut().find(|(name, _)| name == &label) {
                entry.1 += cache_size;
            } else {
                info.caches.push((label, cache_size));
            }
        });
        info.caches.sort_by(|a, b| a.0.cmp(&b.0));
    }
    info
}
