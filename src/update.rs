//! GitHub Releases からの更新確認・ダウンロード・適用。
//! HTTP は WinINet を使い、追加クレートなしで HTTPS / システム プロキシに対応する。

use std::io::Write;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Networking::WinInet::{
    InternetCloseHandle, InternetOpenUrlW, InternetOpenW, InternetReadFile,
    INTERNET_FLAG_RELOAD, INTERNET_OPEN_TYPE_PRECONFIG,
};

pub const RELEASES_API: &str =
    "https://api.github.com/repos/Runa-chama/MiniTask/releases/latest";
pub const CHECK_INTERVAL_SECS: u64 = 86400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn log(msg: &str) {
    if std::env::var_os("MINITASK_DEBUG_UPDATE").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("minitask_update.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 文字列リテラルを `:` の後から取り出す (JSON の簡易パース用)。
fn quoted_after_colon(s: &str) -> Option<(String, usize)> {
    let s = s.trim_start();
    let s = s.strip_prefix(':')?.trim_start();
    if !s.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for (i, ch) in s[1..].char_indices() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some((out, i + 2));
        } else {
            out.push(ch);
        }
    }
    None
}

fn json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = body.find(&needle)?;
    quoted_after_colon(&body[pos + needle.len()..]).map(|(v, _)| v)
}

fn find_asset_url(body: &str) -> Option<String> {
    const NEEDLE: &str = "\"browser_download_url\"";
    let mut offset = 0;
    while let Some(rel) = body[offset..].find(NEEDLE) {
        let start = offset + rel + NEEDLE.len();
        if let Some((value, consumed)) = quoted_after_colon(&body[start..]) {
            let lower = value.to_ascii_lowercase();
            if lower.contains("minitask-") && lower.ends_with("-setup.exe") {
                return Some(value);
            }
            offset = start + consumed;
        } else {
            offset = start;
        }
    }
    None
}

/// `v0.1.2` のようなタグを数値の三部分に変換する。
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let t = s.trim().trim_start_matches(|c| c == 'v' || c == 'V');
    let mut parts = t.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch_text = parts.next().unwrap_or("0");
    let patch = patch_text
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// リモートの方が新しければ true。パース失敗時は安全側 (false)。
pub fn is_newer(remote: &str, current: &str) -> bool {
    match (parse_version(remote), parse_version(current)) {
        (Some(r), Some(c)) => r > c,
        _ => false,
    }
}

unsafe fn http_read(url: &str, mut sink: Option<&mut dyn Write>) -> Result<Vec<u8>, String> {
    unsafe {
        let agent = to_wide("MiniTask");
        let h = InternetOpenW(
            agent.as_ptr(),
            INTERNET_OPEN_TYPE_PRECONFIG,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if h.is_null() {
            return Err(format!("InternetOpen 失敗: {}", std::io::Error::last_os_error()));
        }
        let wide_url = to_wide(url);
        let headers = to_wide("User-Agent: MiniTask\r\nAccept: application/vnd.github+json\r\n");
        let uh = InternetOpenUrlW(
            h,
            wide_url.as_ptr(),
            headers.as_ptr(),
            (headers.len() - 1) as u32,
            INTERNET_FLAG_RELOAD,
            0,
        );
        if uh.is_null() {
            let e = std::io::Error::last_os_error().to_string();
            let _ = InternetCloseHandle(h);
            return Err(format!("InternetOpenUrl 失敗: {e}"));
        }
        let mut result = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let mut read = 0u32;
            if InternetReadFile(
                uh,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
            ) == 0
            {
                let e = std::io::Error::last_os_error().to_string();
                let _ = InternetCloseHandle(uh);
                let _ = InternetCloseHandle(h);
                return Err(format!("InternetReadFile 失敗: {e}"));
            }
            if read == 0 {
                break;
            }
            let chunk = &buf[..read as usize];
            if let Some(w) = sink.as_mut() {
                let _ = w.write_all(chunk);
            }
            result.extend_from_slice(chunk);
        }
        let _ = InternetCloseHandle(uh);
        let _ = InternetCloseHandle(h);
        Ok(result)
    }
}

/// 最新リリースを取得する。開発用に `MINITASK_UPDATE_URL` で API URL を差し替え可能。
pub fn check_latest() -> Option<UpdateInfo> {
    let api = std::env::var("MINITASK_UPDATE_URL").unwrap_or_else(|_| RELEASES_API.to_owned());
    log(&format!("check: {api}"));
    let bytes = match unsafe { http_read(&api, None) } {
        Ok(b) => b,
        Err(e) => {
            log(&format!("check failed: {e}"));
            return None;
        }
    };
    let body = String::from_utf8_lossy(&bytes);
    let version = json_string(&body, "tag_name")?;
    let url = find_asset_url(&body)?;
    log(&format!("found: version={version} url={url}"));
    Some(UpdateInfo { version, url })
}

/// セットアップ.exe を一時ディレクトリへダウンロードする。
pub fn download(info: &UpdateInfo) -> Result<std::path::PathBuf, String> {
    let filename = info
        .url
        .rsplit('/')
        .next()
        .unwrap_or("MiniTask-setup.exe")
        .to_owned();
    let dest = std::env::temp_dir().join(format!(
        "minitask-update-{}-{}",
        info.version.trim_start_matches('v'),
        filename
    ));
    let mut file = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
    let bytes = unsafe { http_read(&info.url, Some(&mut file))? };
    drop(file);
    log(&format!("downloaded {} bytes -> {}", bytes.len(), dest.display()));
    Ok(dest)
}

/// サイレントインストール→自動再起動を行うデタッチしたヘルパーを起動する。
pub fn apply_update(setup: &std::path::Path) -> Result<(), String> {
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_owned());
    let new_exe = format!("{pf}\\MiniTask\\minitask.exe");
    let setup = setup.to_string_lossy().to_string();
    let script = format!(
        "Start-Process -FilePath '{setup}' -ArgumentList '/VERYSILENT','/NORESTART' -Wait; Start-Sleep -Seconds 1; Start-Process -FilePath '{new_exe}'"
    );
    log(&format!("apply: {script}"));
    Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "v0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("bad", "0.1.0"));
        assert_eq!(parse_version("v1.2.3-beta"), Some((1, 2, 3)));
    }

    #[test]
    fn release_json_parsing() {
        let body = r#"{
          "tag_name": "v0.1.1",
          "assets": [
            {"name": "source.zip", "browser_download_url": "https://example.com/source.zip"},
            {"name": "MiniTask-0.1.1-setup.exe", "browser_download_url": "https://example.com/MiniTask-0.1.1-setup.exe"}
          ]
        }"#;
        assert_eq!(json_string(body, "tag_name").as_deref(), Some("v0.1.1"));
        assert_eq!(
            find_asset_url(body).as_deref(),
            Some("https://example.com/MiniTask-0.1.1-setup.exe")
        );
    }
}
