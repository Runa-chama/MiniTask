//! スタートアップ (自動起動) 管理。
//!
//! 対象: HKCU/HKLM の Run キー、HKLM の WOW6432Node\Run、
//! ユーザー/共通スタートアップフォルダー。
//! 有効/無効はタスクマネージャーと同じ `Explorer\StartupApproved` キー方式。

use std::borrow::Cow;

use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_BINARY,
};
use winreg::{RegKey, RegValue};

const RUN_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32_PATH: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_BASE: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartupKind {
    HkcuRun,
    HklmRun,
    HklmRun32,
    UserStartup,
    CommonStartup,
}

impl StartupKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::HkcuRun => "レジストリ (HKCU)",
            Self::HklmRun => "レジストリ (HKLM)",
            Self::HklmRun32 => "レジストリ (HKLM 32bit)",
            Self::UserStartup => "フォルダー (ユーザー)",
            Self::CommonStartup => "フォルダー (共通)",
        }
    }

    fn approved_subkey(self) -> &'static str {
        match self {
            Self::HkcuRun | Self::HklmRun => "Run",
            Self::HklmRun32 => "Run32",
            Self::UserStartup | Self::CommonStartup => "StartupFolder",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StartupEntry {
    pub name: String,
    pub command: String,
    pub kind: StartupKind,
    pub enabled: bool,
}

fn folder_dir(kind: StartupKind) -> Option<String> {
    let base = match kind {
        StartupKind::UserStartup => std::env::var("APPDATA").ok()?,
        StartupKind::CommonStartup => std::env::var("ProgramData").ok()?,
        _ => return None,
    };
    Some(format!(
        r"{base}\Microsoft\Windows\Start Menu\Programs\Startup"
    ))
}

/// StartupApproved の値から有効/無効を判定 (先頭バイト 0x03 = 無効)。
/// 値は種別ごとのサブキー (Run / Run32 / StartupFolder) の下にある。
fn is_enabled(approved_base: &RegKey, kind: StartupKind, name: &str) -> bool {
    let Ok(key) = approved_base.open_subkey(kind.approved_subkey()) else {
        return true;
    };
    match key.get_raw_value(name) {
        Ok(v) => v.bytes.first() != Some(&3),
        Err(_) => true, // 未登録 = 有効
    }
}

fn read_reg_entries(
    hive: winreg::HKEY,
    path: &str,
    kind: StartupKind,
    approved: &RegKey,
    out: &mut Vec<StartupEntry>,
) {
    let Ok(key) = RegKey::predef(hive).open_subkey_with_flags(path, KEY_READ | KEY_WOW64_64KEY)
    else {
        return;
    };
    for (name, value) in key.enum_values().flatten() {
        if name.is_empty() {
            continue;
        }
        out.push(StartupEntry {
            command: value.to_string(),
            enabled: is_enabled(approved, kind, &name),
            name,
            kind,
        });
    }
}

fn read_folder_entries(kind: StartupKind, approved: &RegKey, out: &mut Vec<StartupEntry>) {
    let Some(dir) = folder_dir(kind) else { return };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        out.push(StartupEntry {
            command: path.to_string_lossy().into_owned(),
            enabled: is_enabled(approved, kind, &name),
            name,
            kind,
        });
    }
}

/// 全スタートアップエントリを列挙。
pub fn list() -> Vec<StartupEntry> {
    let mut out = Vec::new();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let approved = match hkcu.create_subkey(APPROVED_BASE) {
        Ok((key, _)) => key,
        Err(_) => match hkcu.open_subkey(APPROVED_BASE) {
            Ok(key) => key,
            Err(_) => return out,
        },
    };

    read_reg_entries(HKEY_CURRENT_USER, RUN_PATH, StartupKind::HkcuRun, &approved, &mut out);
    read_reg_entries(HKEY_LOCAL_MACHINE, RUN_PATH, StartupKind::HklmRun, &approved, &mut out);
    read_reg_entries(HKEY_LOCAL_MACHINE, RUN32_PATH, StartupKind::HklmRun32, &approved, &mut out);
    read_folder_entries(StartupKind::UserStartup, &approved, &mut out);
    read_folder_entries(StartupKind::CommonStartup, &approved, &mut out);

    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// 有効/無効を切り替える (StartupApproved に 12 バイトのバイナリ値を書く)。
pub fn set_enabled(entry: &StartupEntry, enabled: bool) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = format!(r"{APPROVED_BASE}\{}", entry.kind.approved_subkey());
    let (key, _) = hkcu.create_subkey(&path).map_err(|e| e.to_string())?;

    let mut bytes = [0u8; 12];
    if enabled {
        bytes[0] = 2; // 有効
    } else {
        bytes[0] = 3; // 無効 + 無効化時刻 (FILETIME)
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ticks = (unix + 11_644_473_600).saturating_mul(10_000_000);
        bytes[4..12].copy_from_slice(&ticks.to_le_bytes());
    }
    key.set_raw_value(
        &entry.name,
        &RegValue {
            bytes: Cow::Borrowed(&bytes),
            vtype: REG_BINARY,
        },
    )
    .map_err(|e| e.to_string())
}

/// エントリを削除する (レジストリ値またはスタートアップフォルダー内のファイル)。
pub fn delete(entry: &StartupEntry) -> Result<(), String> {
    let result = match entry.kind {
        StartupKind::HkcuRun => delete_reg_value(HKEY_CURRENT_USER, RUN_PATH, &entry.name),
        StartupKind::HklmRun => delete_reg_value(HKEY_LOCAL_MACHINE, RUN_PATH, &entry.name),
        StartupKind::HklmRun32 => delete_reg_value(HKEY_LOCAL_MACHINE, RUN32_PATH, &entry.name),
        StartupKind::UserStartup | StartupKind::CommonStartup => {
            let dir = folder_dir(entry.kind).ok_or("フォルダーパスを取得できません")?;
            std::fs::remove_file(format!(r"{dir}\{}", entry.name)).map_err(|e| e.to_string())
        }
    };
    // 承認状態の値も掃除 (失敗しても本題には影響しない)
    if result.is_ok() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey(format!(r"{APPROVED_BASE}\{}", entry.kind.approved_subkey())) {
            let _ = key.delete_value(&entry.name);
        }
    }
    result
}

fn delete_reg_value(hive: winreg::HKEY, path: &str, name: &str) -> Result<(), String> {
    let key = RegKey::predef(hive)
        .open_subkey_with_flags(path, winreg::enums::KEY_SET_VALUE | KEY_WOW64_64KEY)
        .map_err(|e| e.to_string())?;
    key.delete_value(name).map_err(|e| e.to_string())
}
