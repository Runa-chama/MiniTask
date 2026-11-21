//! プロセス情報の収集 (sysinfo + Win32) とプロセス終了。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use sysinfo::{CpuRefreshKind, Disks, Gpus, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, LUID};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
    LookupAccountSidW, LookupPrivilegeValueW, AdjustTokenPrivileges, LUID_AND_ATTRIBUTES,
    SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, TerminateProcess, PROCESS_TERMINATE,
};

use crate::net::{self, NetCounters};
use crate::perf::NpuMeter;

/// UI に表示する 1 プロセス分の情報。
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
    /// マシン全体に対する CPU 使用率 (0..100, タスクマネージャー準拠)
    pub cpu: f32,
    /// ワーキングセット (バイト)
    pub mem: u64,
    pub io_read: u64,  // bytes/sec
    pub io_write: u64, // bytes/sec
    pub net_recv: u64, // bytes/sec
    pub net_sent: u64, // bytes/sec
    pub exe: String,
    pub cmd: String,
    pub user: String,
    pub start_time: u64, // UNIX 秒
}

/// システム全体の負荷。
#[derive(Clone, Debug, Default)]
pub struct SysLoad {
    pub cpu: f32, // 0..100
    pub mem_used: u64,
    pub mem_total: u64,
    pub rom: f32, // ストレージ使用率 0..100
    pub gpu: f32, // GPU 使用率 0..100
    pub npu: f32, // NPU 使用率 0..100
    pub npu_available: bool,

    // --- パフォーマンスタブ向けの詳細情報 ---
    pub cpu_brand: String,
    pub cpu_freq_mhz: u64,
    pub logical_cores: usize,
    pub mem_available: u64,
    pub commit_used: u64,
    pub commit_limit: u64,
    pub cached: u64,
    pub rom_read: u64, // bytes/sec
    pub rom_written: u64,
    pub rom_free: u64,
    pub rom_total: u64,
    pub gpu_model: String,
    pub gpu_mem_used: u64,
    pub gpu_mem_total: u64,
    pub processes: u32,
    pub threads: u32,
    pub handles: u32,
    pub uptime_secs: u64,
}

fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet)
        .without_tasks()
}

pub struct ProcCollector {
    sys: System,
    cpu_count: f32,
    prev_net: HashMap<u32, NetCounters>,
    prev_time: Instant,
    user_cache: HashMap<String, String>,
    disks: Disks,
    gpus: Gpus,
    npu: NpuMeter,
}

impl ProcCollector {
    pub fn new() -> Result<Self, String> {
        let mut sys = System::new().map_err(|e| format!("システム情報の初期化に失敗: {e}"))?;
        // ブランド名・周波数も取得しておく
        sys.refresh_cpu_specifics(CpuRefreshKind::everything());
        let cpu_count = sys.cpus().len().max(1) as f32;
        sys.refresh_memory();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());
        let disks = Disks::new_with_refreshed_list().unwrap_or_else(|_| Disks::new().unwrap());
        let gpus = Gpus::new_with_refreshed_list().unwrap_or_else(|_| Gpus::new().unwrap());
        Ok(Self {
            sys,
            cpu_count,
            prev_net: net::snapshot(),
            prev_time: Instant::now(),
            user_cache: HashMap::new(),
            disks,
            gpus,
            npu: NpuMeter::new(),
        })
    }

    /// 全プロセスを再取得し、前回からの差分で速度系を計算する。
    pub fn refresh(&mut self) -> (Vec<ProcRow>, SysLoad) {
        self.sys.refresh_cpu_specifics(
            CpuRefreshKind::nothing().with_usage().with_frequency(),
        );
        self.sys.refresh_memory();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());

        let dt = self.prev_time.elapsed().as_secs_f64().max(0.05);
        self.prev_time = Instant::now();

        // --- ストレージ (ROM): C: ドライブを優先、なければ最初のディスク ---
        self.disks.refresh(false);
        let (rom, rom_read, rom_written, rom_free, rom_total) = self
            .disks
            .iter()
            .find(|d| d.mount_point().to_string_lossy().starts_with("C:"))
            .or_else(|| self.disks.iter().next())
            .map(|d| {
                let total = d.total_space();
                let pct = if total > 0 {
                    ((total - d.available_space().min(total)) as f64 / total as f64 * 100.0) as f32
                } else {
                    0.0
                };
                let du = d.usage();
                (
                    pct,
                    (du.read_bytes as f64 / dt) as u64,
                    (du.written_bytes as f64 / dt) as u64,
                    d.available_space(),
                    total,
                )
            })
            .unwrap_or((0.0, 0, 0, 0, 0));

        // --- GPU: 最も使用率の高い GPU を主 GPU として採用 ---
        self.gpus.refresh(false);
        let gpu = self
            .gpus
            .iter()
            .filter_map(|g| g.usage())
            .fold(0.0_f32, f32::max)
            .clamp(0.0, 100.0);
        let primary_gpu = self
            .gpus
            .iter()
            .find(|g| g.usage().is_some())
            .or_else(|| self.gpus.iter().next());
        let gpu_model = primary_gpu
            .and_then(|g| g.model())
            .unwrap_or("不明")
            .to_owned();
        let gpu_mem_used = primary_gpu.and_then(|g| g.used_memory()).unwrap_or(0);
        let gpu_mem_total = primary_gpu.and_then(|g| g.total_memory()).unwrap_or(0);

        // --- NPU (非搭載機では 0) ---
        let npu_available = self.npu.available;
        let npu = self.npu.usage().unwrap_or(0.0);

        // --- CPU 詳細 ---
        let cpu_brand = self
            .sys
            .cpus()
            .first()
            .map(|c| c.brand().to_owned())
            .unwrap_or_default();
        let cpu_freq_mhz = self
            .sys
            .cpus()
            .iter()
            .map(sysinfo::Cpu::frequency)
            .max()
            .unwrap_or(0);
        let logical_cores = self.sys.cpus().len();

        // --- システム全体の統計 (プロセス/スレッド/ハンドル/コミット) ---
        let (mem_available, commit_used, commit_limit, cached, processes, threads, handles) =
            unsafe {
                let size = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
                let mut pi = PERFORMANCE_INFORMATION {
                    cb: size,
                    ..Default::default()
                };
                if GetPerformanceInfo(&mut pi, size) != 0 {
                    let ps = pi.PageSize as u64;
                    (
                        pi.PhysicalAvailable as u64 * ps,
                        pi.CommitTotal as u64 * ps,
                        pi.CommitLimit as u64 * ps,
                        pi.SystemCache as u64 * ps,
                        pi.ProcessCount,
                        pi.ThreadCount,
                        pi.HandleCount,
                    )
                } else {
                    (0, 0, 0, 0, 0, 0, 0)
                }
            };
        let uptime_secs = unsafe { GetTickCount64() / 1000 };

        let load = SysLoad {
            cpu: self.sys.global_cpu_usage(),
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            rom,
            gpu,
            npu,
            npu_available,
            cpu_brand,
            cpu_freq_mhz,
            logical_cores,
            mem_available,
            commit_used,
            commit_limit,
            cached,
            rom_read,
            rom_written,
            rom_free,
            rom_total,
            gpu_model,
            gpu_mem_used,
            gpu_mem_total,
            processes,
            threads,
            handles,
            uptime_secs,
        };

        let now_net = net::snapshot();

        let mut rows = Vec::with_capacity(self.sys.processes().len());
        let mut live = HashSet::with_capacity(self.sys.processes().len());
        for (pid, p) in self.sys.processes() {
            let pid_u = pid.as_u32();
            live.insert(pid_u);
            let prev = self.prev_net.get(&pid_u).copied().unwrap_or_default();
            let cur = now_net.get(&pid_u).copied().unwrap_or_default();
            let disk = p.disk_usage();

            let user = p
                .user_id()
                .map(|uid| {
                    let sid = uid.to_string();
                    if let Some(name) = self.user_cache.get(&sid) {
                        name.clone()
                    } else {
                        let name = lookup_user(&sid).unwrap_or_else(|| sid.clone());
                        self.user_cache.insert(sid, name.clone());
                        name
                    }
                })
                .unwrap_or_default();

            rows.push(ProcRow {
                pid: pid_u,
                name: p.name().to_string_lossy().into_owned(),
                cpu: (p.cpu_usage() / self.cpu_count).clamp(0.0, 100.0),
                mem: p.memory(),
                io_read: (disk.read_bytes as f64 / dt) as u64,
                io_write: (disk.written_bytes as f64 / dt) as u64,
                net_recv: (cur.recv.saturating_sub(prev.recv) as f64 / dt) as u64,
                net_sent: (cur.sent.saturating_sub(prev.sent) as f64 / dt) as u64,
                exe: p
                    .exe()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                user,
                start_time: p.start_time(),
            });
        }
        self.prev_net = now_net;
        net::prune(&live);
        (rows, load)
    }
}

/// プロセスを強制終了する。
pub fn kill(pid: u32) -> Result<(), String> {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(format!(
                "プロセスを開けません (PID {pid}, エラー {})",
                GetLastError()
            ));
        }
        let ok = TerminateProcess(handle, 1);
        let err = GetLastError();
        CloseHandle(handle);
        if ok == 0 {
            Err(format!("PID {pid} の終了に失敗しました (エラー {err})"))
        } else {
            Ok(())
        }
    }
}

/// SeDebugPrivilege を有効化 (他ユーザー/一部システムプロセスの照会・終了用)。
pub fn enable_debug_privilege() {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return;
        }
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if LookupPrivilegeValueW(std::ptr::null(), SE_DEBUG_NAME, &mut luid) != 0 {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
        }
        CloseHandle(token);
    }
}

/// SID 文字列 → アカウント名 (例: "COMPUTER\user" → "user")。
fn lookup_user(sid_str: &str) -> Option<String> {
    unsafe {
        let wide: Vec<u16> = sid_str.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psid = std::ptr::null_mut();
        if ConvertStringSidToSidW(wide.as_ptr(), &mut psid) == 0 {
            return None;
        }
        let mut name_len = 0u32;
        let mut domain_len = 0u32;
        let mut sid_use = 0i32;
        // 1 回目で必要バッファサイズを取得
        LookupAccountSidW(
            std::ptr::null(),
            psid,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut sid_use,
        );
        if name_len == 0 {
            LocalFree(psid);
            return None;
        }
        let mut name = vec![0u16; name_len as usize];
        // ドメインバッファも渡さないと ERROR_INSUFFICIENT_BUFFER で失敗する
        let mut domain = vec![0u16; (domain_len as usize).max(1)];
        let ok = LookupAccountSidW(
            std::ptr::null(),
            psid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut sid_use,
        );
        LocalFree(psid);
        if ok == 0 {
            return None;
        }
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Some(String::from_utf16_lossy(&name[..end]))
    }
}
