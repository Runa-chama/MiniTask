//! ネットワーク アダプター別の送受信速度と詳細情報。
//!
//! - PDH `\Network Interface(*)\Bytes Received/sec` / `Bytes Sent/sec` で
//!   アダプターごとの送受信速度を取得
//! - iphlpapi `GetAdaptersAddresses` で表示名・IP・帯域幅・状態を取得
//! - wlanapi で Wi-Fi の SSID と信号の品質を取得

use std::collections::HashMap;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::WiFi::{
    WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle, WlanQueryInterface,
    wlan_intf_opcode_current_connection, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST,
};
use windows_sys::Win32::Networking::WinSock::{
    InetNtopW, AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6,
};
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW, PDH_HCOUNTER,
    PDH_HQUERY,
};

use crate::perf::collect_counter_array;

/// ネットワーク アダプター 1 台分の情報。
#[derive(Clone, Debug, Default)]
pub struct AdapterSample {
    /// PDH インスタンス名 (安定したキー)
    pub id: String,
    /// 表示名 (例: "Wi-Fi", "イーサネット 7")
    pub label: String,
    /// アダプターの説明
    pub description: String,
    pub is_wifi: bool,
    pub recv_bps: u64,
    pub sent_bps: u64,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub ipv4_global: Vec<String>,
    pub ipv4_local: Vec<String>,
    pub ipv6_global: Vec<String>,
    pub ipv6_local: Vec<String>,
    /// リンク速度 (bits/sec)
    pub link_speed_bps: u64,
    /// 状態
    pub status: String,
    /// Wi-Fi の接続先 SSID
    pub ssid: String,
    /// Wi-Fi の信号の品質 (0..100)
    pub signal_pct: u32,
    /// Wi-Fi の規格 (例: "802.11ax")
    pub wifi_standard: Option<String>,
    /// 推定 RSSI (dBm)
    pub signal_dbm: Option<i32>,
}

#[derive(Clone, Default)]
struct IpInfo {
    friendly: String,
    description: String,
    is_wifi: bool,
    ipv4: Vec<String>,
    ipv6: Vec<String>,
    ipv4_global: Vec<String>,
    ipv4_local: Vec<String>,
    ipv6_global: Vec<String>,
    ipv6_local: Vec<String>,
    link_speed_bps: u64,
    status: String,
}

#[derive(Default)]
struct WifiInfo {
    ssid: String,
    signal_pct: u32,
    standard: Option<String>,
}

impl WifiInfo {
    /// 信号品質からおおよその RSSI を求める (100% ≈ -50 dBm)。
    fn signal_dbm(&self) -> i32 {
        -100 + (self.signal_pct.min(100) as i32) / 2
    }
}

/// 802.11 PHY タイプを規格名に変換する。
fn phy_type_name(value: i32) -> Option<&'static str> {
    match value {
        1 => Some("802.11b"),
        2 => Some("802.11a"),
        3 => Some("802.11g"),
        4 => Some("802.11n"),
        5 => Some("802.11ac"),
        6 => Some("802.11ax"),
        7 => Some("802.11be"),
        _ => None,
    }
}

/// IPv4 がローカル (私用/リンクローカル) かどうか。
fn ipv4_is_local(text: &str) -> bool {
    let parts: Vec<u8> = text.split('.').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 4 {
        return false;
    }
    let (a, b) = (parts[0], parts[1]);
    a == 10
        || a == 127
        || a == 0
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
}

/// IPv6 がローカル (リンクローカル/一意ローカル/ループバック) かどうか。
fn ipv6_is_local(text: &str) -> bool {
    let s = text.to_ascii_lowercase();
    s.starts_with("fe80:")
        || s.starts_with("fc")
        || s.starts_with("fd")
        || s.starts_with("::1")
        || s == "::"
}

/// PDH + iphlpapi + wlanapi をまとめるメータ。
pub struct NetAdapterMeter {
    query: PDH_HQUERY,
    recv_counter: PDH_HCOUNTER,
    sent_counter: PDH_HCOUNTER,
    pub available: bool,
}

impl NetAdapterMeter {
    pub fn new() -> Self {
        unsafe {
            let mut query: PDH_HQUERY = std::ptr::null_mut();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != ERROR_SUCCESS {
                return Self::unavailable();
            }
            let mut counters = Vec::new();
            for name in [
                r"\Network Interface(*)\Bytes Received/sec",
                r"\Network Interface(*)\Bytes Sent/sec",
            ] {
                let path: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
                if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != ERROR_SUCCESS {
                    PdhCloseQuery(query);
                    return Self::unavailable();
                }
                counters.push(counter);
            }
            PdhCollectQueryData(query);
            Self {
                query,
                recv_counter: counters[0],
                sent_counter: counters[1],
                available: true,
            }
        }
    }

    fn unavailable() -> Self {
        Self {
            query: std::ptr::null_mut(),
            recv_counter: std::ptr::null_mut(),
            sent_counter: std::ptr::null_mut(),
            available: false,
        }
    }

    /// 全アダプターの現在の使用率・詳細を返す。
    pub fn read(&mut self) -> Vec<AdapterSample> {
        if !self.available {
            return Vec::new();
        }
        unsafe {
            PdhCollectQueryData(self.query);
            let recvs = collect_counter_array(self.recv_counter).unwrap_or_default();
            let sents = collect_counter_array(self.sent_counter).unwrap_or_default();
            let sent_map: HashMap<String, f64> = sents.into_iter().collect();
            let ip_infos = ip_adapters();
            let wifi_infos = wifi_adapters();

            let mut samples: Vec<AdapterSample> = recvs
                .into_iter()
                .filter(|(name, _)| !name.is_empty() && !name.eq_ignore_ascii_case("_total"))
                .filter_map(|(name, recv)| {
                    let key = name.to_lowercase();
                    let info = ip_infos.get(&key);
                    let description = info
                        .map(|i| i.description.clone())
                        .unwrap_or_else(|| name.clone());
                    let wifi = wifi_infos.get(&description.to_lowercase());
                    let is_wifi = info.map(|i| i.is_wifi).unwrap_or_else(|| {
                        let n = name.to_lowercase();
                        n.contains("wi-fi") || n.contains("wifi") || n.contains("wireless")
                    });
                    let label = info
                        .map(|i| i.friendly.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| name.clone());
                    let sent = sent_map.get(&name).copied().unwrap_or(0.0);
                    Some(AdapterSample {
                        id: name,
                        label,
                        description,
                        is_wifi,
                        recv_bps: recv.max(0.0) as u64,
                        sent_bps: sent.max(0.0) as u64,
                        ipv4: info.map(|i| i.ipv4.clone()).unwrap_or_default(),
                        ipv6: info.map(|i| i.ipv6.clone()).unwrap_or_default(),
                        ipv4_global: info.map(|i| i.ipv4_global.clone()).unwrap_or_default(),
                        ipv4_local: info.map(|i| i.ipv4_local.clone()).unwrap_or_default(),
                        ipv6_global: info.map(|i| i.ipv6_global.clone()).unwrap_or_default(),
                        ipv6_local: info.map(|i| i.ipv6_local.clone()).unwrap_or_default(),
                        link_speed_bps: info.map(|i| i.link_speed_bps).unwrap_or(0),
                        status: info
                            .map(|i| i.status.clone())
                            .unwrap_or_else(|| "不明".to_owned()),
                        ssid: wifi.map(|w| w.ssid.clone()).unwrap_or_default(),
                        signal_pct: wifi.map(|w| w.signal_pct).unwrap_or(0),
                        wifi_standard: wifi.and_then(|w| w.standard.clone()),
                        signal_dbm: wifi.map(|w| w.signal_dbm()),
                    })
                })
                .collect();
            // 同じ表示名のアダプターには連番を付与
            let mut counts: HashMap<String, usize> = HashMap::new();
            for s in &mut samples {
                let n = counts.entry(s.label.clone()).or_insert(0);
                *n += 1;
                if *n > 1 {
                    s.label = format!("{} {}", s.label, *n);
                }
            }
            samples.sort_by(|a, b| a.label.cmp(&b.label));
            samples
        }
    }
}

impl Drop for NetAdapterMeter {
    fn drop(&mut self) {
        if !self.query.is_null() {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}

/// GetAdaptersAddresses からアダプター詳細を取得 (説明名と表示名の両方をキーにする)。
fn ip_adapters() -> HashMap<String, IpInfo> {
    let mut map: HashMap<String, IpInfo> = HashMap::new();
    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = 0u32;
        let status = GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null(), std::ptr::null_mut(), &mut size);
        if status != ERROR_BUFFER_OVERFLOW || size == 0 {
            return map;
        }
        let mut buffer = vec![0u8; size as usize];
        let status = GetAdaptersAddresses(
            AF_INET as u32,
            flags,
            std::ptr::null(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        );
        if status != ERROR_SUCCESS {
            return map;
        }
        let mut current = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !current.is_null() {
            let a = &*current;
            let friendly = crate::perf::wide_ptr_to_string(a.FriendlyName);
            let description = crate::perf::wide_ptr_to_string(a.Description);
            let mut ipv4 = Vec::new();
            let mut ipv6 = Vec::new();
            let mut ipv4_global = Vec::new();
            let mut ipv4_local = Vec::new();
            let mut ipv6_global = Vec::new();
            let mut ipv6_local = Vec::new();
            let mut addr = a.FirstUnicastAddress;
            while !addr.is_null() {
                let u = &*addr;
                if !u.Address.lpSockaddr.is_null() {
                    let family = (*u.Address.lpSockaddr.cast::<SOCKADDR_IN>()).sin_family;
                    let name = if family == AF_INET {
                        let p = &(*u.Address.lpSockaddr.cast::<SOCKADDR_IN>()).sin_addr;
                        inet_ntop(AF_INET, p as *const _ as *const std::ffi::c_void)
                    } else if family == AF_INET6 {
                        let p = &(*u.Address.lpSockaddr.cast::<SOCKADDR_IN6>()).sin6_addr;
                        inet_ntop(AF_INET6, p as *const _ as *const std::ffi::c_void)
                    } else {
                        String::new()
                    };
                    if !name.is_empty() {
                        if family == AF_INET {
                            if ipv4_is_local(&name) {
                                ipv4_local.push(name.clone());
                            } else {
                                ipv4_global.push(name.clone());
                            }
                            ipv4.push(name);
                        } else {
                            if ipv6_is_local(&name) {
                                ipv6_local.push(name.clone());
                            } else {
                                ipv6_global.push(name.clone());
                            }
                            ipv6.push(name);
                        }
                    }
                }
                addr = u.Next;
            }
            let info = IpInfo {
                friendly: friendly.clone(),
                description: description.clone(),
                is_wifi: a.IfType == IF_TYPE_IEEE80211,
                ipv4,
                ipv6,
                ipv4_global,
                ipv4_local,
                ipv6_global,
                ipv6_local,
                link_speed_bps: a.TransmitLinkSpeed.max(a.ReceiveLinkSpeed),
                status: if a.OperStatus == 1 {
                    "稼働中".to_owned()
                } else {
                    "切断".to_owned()
                },
            };
            let key = description.to_lowercase();
            map.insert(key.clone(), info.clone());
            if !friendly.is_empty() {
                map.entry(friendly.to_lowercase()).or_insert(info);
            }
            current = a.Next;
        }
    }
    map
}

fn inet_ntop(family: u16, addr: *const std::ffi::c_void) -> String {
    let mut buf = [0u16; 64];
    let result = unsafe { InetNtopW(family as i32, addr, buf.as_mut_ptr(), buf.len()) };
    if result.is_null() {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// wlanapi から Wi-Fi の SSID (プロファイル名) と信号の品質を取得。
fn wifi_adapters() -> HashMap<String, WifiInfo> {
    let mut map = HashMap::new();
    unsafe {
        let mut handle: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
        let mut version = 0u32;
        if WlanOpenHandle(2, std::ptr::null(), &mut version, &mut handle) != ERROR_SUCCESS {
            return map;
        }
        let mut list_ptr: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(handle, std::ptr::null(), &mut list_ptr) != ERROR_SUCCESS {
            WlanCloseHandle(handle, std::ptr::null());
            return map;
        }
        let count = (*list_ptr).dwNumberOfItems as usize;
        let base = (*list_ptr).InterfaceInfo.as_ptr();
        for i in 0..count {
            let intf = &*base.add(i);
            let desc = u16_array(&intf.strInterfaceDescription);
            let mut size = 0u32;
            let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
            let status = WlanQueryInterface(
                handle,
                &intf.InterfaceGuid,
                wlan_intf_opcode_current_connection,
                std::ptr::null(),
                &mut size,
                &mut data,
                std::ptr::null_mut(),
            );
            if status == ERROR_SUCCESS && !data.is_null() {
                let conn = &*data.cast::<WLAN_CONNECTION_ATTRIBUTES>();
                map.insert(
                    desc.to_lowercase(),
                    WifiInfo {
                        ssid: u16_array(&conn.strProfileName),
                        signal_pct: conn.wlanAssociationAttributes.wlanSignalQuality,
                        standard: phy_type_name(
                            conn.wlanAssociationAttributes.dot11PhyType as i32,
                        )
                        .map(str::to_owned),
                    },
                );
                WlanFreeMemory(data);
            }
        }
        WlanFreeMemory(list_ptr.cast());
        WlanCloseHandle(handle, std::ptr::null());
    }
    map
}

fn u16_array(chars: &[u16]) -> String {
    let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    String::from_utf16_lossy(&chars[..len])
}
