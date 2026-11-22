//! SMBIOS からメモリのフォームファクタとスロット数を取得する。

use windows_sys::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

#[derive(Clone, Debug, Default)]
pub struct MemoryInfo {
    pub form_factor: Option<String>,
    pub slots_used: Option<u16>,
    pub slots_total: Option<u16>,
}

fn form_factor_name(code: u8) -> &'static str {
    match code {
        0x03 => "SIMM",
        0x04 => "SIP",
        0x05 => "Chip",
        0x06 => "DIP",
        0x07 => "ZIP",
        0x08 => "Proprietary Card",
        0x09 => "DIMM",
        0x0A => "TSOP",
        0x0B => "Row of chips",
        0x0C => "RIMM",
        0x0D => "SO-DIMM",
        0x0E => "SRIMM",
        0x0F => "FB-DIMM",
        _ => "その他",
    }
}

pub fn query() -> MemoryInfo {
    let mut info = MemoryInfo::default();
    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0) as usize;
        if size == 0 {
            return info;
        }
        let mut buf = vec![0u8; size];
        let written = GetSystemFirmwareTable(
            RSMB,
            0,
            buf.as_mut_ptr(),
            size as u32,
        ) as usize;
        if written == 0 || written > buf.len() {
            return info;
        }
        buf.truncate(written);

        // Raw SMBIOS の先頭には Entry Point が入ることがあるため、
        // 最初の Type 0 (BIOS Information) を探してそこから構造体を辿る。
        let mut start = None;
        for i in 0..buf.len().saturating_sub(2) {
            if buf[i] == 0 && buf[i + 1] >= 0x14 && i + buf[i + 1] as usize <= buf.len() {
                start = Some(i);
                break;
            }
        }
        let mut off = match start {
            Some(v) => v,
            None => return info,
        };

        while off + 4 <= buf.len() {
            let ty = buf[off];
            let len = buf[off + 1] as usize;
            if len < 4 || off + len > buf.len() {
                break;
            }
            let data = &buf[off..off + len];
            match ty {
                0x10 if len >= 0x0F => {
                    // Physical Memory Array: NumberOfMemoryDevices (offset 0x0D)
                    let slots = u16::from_le_bytes([data[0x0D], data[0x0E]]);
                    if slots > 0 {
                        info.slots_total = Some(slots);
                    }
                }
                0x11 if len >= 0x0F => {
                    // Memory Device: Size (0x0C), Form Factor (0x0E)
                    let size = u16::from_le_bytes([data[0x0C], data[0x0D]]);
                    if size != 0 && size != 0xFFFF {
                        info.slots_used = Some(info.slots_used.unwrap_or(0) + 1);
                        if info.form_factor.is_none() {
                            info.form_factor = Some(form_factor_name(data[0x0E]).to_owned());
                        }
                    }
                }
                0x7F => break, // End-of-Table
                _ => {}
            }

            // 構造体の後には文字列テーブルが続く (末尾は NULL NULL)
            let mut cursor = off + len;
            let mut nulls = 0;
            while cursor < buf.len() {
                if buf[cursor] == 0 {
                    nulls += 1;
                    if nulls >= 2 {
                        cursor += 1;
                        break;
                    }
                } else {
                    nulls = 0;
                }
                cursor += 1;
            }
            off = cursor;
        }
    }
    info
}
