//! プロセス情報の収集 (sysinfo + Win32) とプロセス終了。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::netadapter::NetAdapterMeter;
use crate::perf::{DiskMeter, EngineMeter, GpuMemoryMeter, GPU_ENGINE_COUNTER, NPU_ENGINE_COUNTER};
use crate::smart::{self, SmartInfo};

/// UI に表示する 1 プロセス分の情報。
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    /// 実行状態 ("実行中" など)
    pub status: String,
    /// マシン全体に対する CPU 使用率 (0..100, タスクマネージャー準拠)
    pub cpu: f32,
    /// ワーキングセット (バイト)
    pub mem: u64,
    /// プロセス単位の GPU 使用率 (0..100)
    pub gpu: f32,
    pub io_read: u64,  // bytes/sec
    pub io_write: u64, // bytes/sec
    pub net_recv: u64, // bytes/sec
    pub net_sent: u64, // bytes/sec
    pub exe: String,
    pub cmd: String,
    pub user: String,
    pub start_time: u64, // UNIX 秒
}

/// ストレージ デバイス 1 台分の情報。
#[derive(Clone, Debug, Default)]
pub struct DiskInfo {
    /// 物理ディスク番号 (PDH インスタンス名の先頭数字)
    pub id: String,
    /// 表示名 (例: "ディスク 2 (D: E:)")
    pub label: String,
    /// この物理ディスクに属するドライブレター
    pub letters: Vec<String>,
    pub read_bps: u64,
    pub write_bps: u64,
    /// アクティブな時間 (0..100)
    pub active_pct: f32,
    /// 平均応答時間 (ミリ秒)
    pub response_ms: f32,
    /// 種類 (例: "リムーバブル (USB)")
    pub type_label: String,
    pub free: u64,
    pub total: u64,
    /// SMART 健康状態 (取得できた場合)
    pub smart: Option<Arc<SmartInfo>>,
}

/// GPU 1 台分の情報。
#[derive(Clone, Debug, Default)]
pub struct GpuInfo {
    /// 列挙順の ID (例: "0")
    pub id: String,
    /// 表示名 (例: "GPU 0")
    pub label: String,
    pub model: String,
    pub usage: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    /// エンジン種別ごとの使用率 (複数 GPU の場合は全 GPU の合計)
    pub engines: Vec<(String, f32)>,
}

/// NPU 1 台分の情報。
#[derive(Clone, Debug, Default)]
pub struct NpuInfo {
    /// PDH カウンタの LUID
    pub id: String,
    /// 表示名 (例: "NPU 0")
    pub label: String,
    pub name: String,
    pub usage: f32,
    /// エンジン種別ごとの使用率
    pub engines: Vec<(String, f32)>,
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
    /// NPU 使用率カウンタが利用可能か
    pub npu_available: bool,
    /// NPU デバイス自体が搭載されているか (カウンタが無くても true になり得る)
    pub npu_device_present: bool,
    /// 論理プロセッサごとの使用率 (0..100)
    pub core_usage: Vec<f32>,
    /// GPU エンジン種別ごとの使用率 (キーは engtype の小文字)
    pub gpu_engines: Vec<(String, f32)>,
    /// NPU エンジン種別ごとの使用率
    pub npu_engines: Vec<(String, f32)>,
    /// 接続中のストレージ一覧 (切断時は自動的に消える)
    pub disks: Vec<DiskInfo>,
    /// 搭載中の GPU 一覧
    pub gpus: Vec<GpuInfo>,
    /// 搭載中の NPU 一覧
    pub npus: Vec<NpuInfo>,
    /// 接続中のネットワーク アダプター一覧
    pub adapters: Vec<crate::netadapter::AdapterSample>,
    /// GPU/NPU の専用・共有メモリ使用量 (bytes)
    pub gpu_dedicated: u64,
    pub gpu_shared: u64,
    pub npu_dedicated: u64,
    pub npu_shared: u64,

    // --- パフォーマンスタブ向けの詳細情報 ---
    pub cpu_brand: String,
    pub cpu_freq_mhz: u64,
    pub logical_cores: usize,
    pub mem_available: u64,
    pub commit_used: u64,
    pub commit_limit: u64,
    pub cached: u64,
    pub kernel_paged: u64,
    pub kernel_nonpaged: u64,
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
        .with_gpu_usage()
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
    disk_meter: DiskMeter,
    net_meter: NetAdapterMeter,
    gpu_memory_meter: GpuMemoryMeter,
    npu: EngineMeter,
    gpu_engine_meter: EngineMeter,
    npu_device_present: bool,
    /// カウンタ未開の NPU を定期的に開き直すためのリフレッシュ回数カウンタ
    npu_recheck: u32,
    /// SMART 情報のキャッシュ (60秒TTL)
    smart_cache: HashMap<String, (Instant, Arc<SmartInfo>)>,
}

/// グラフ描画用の軽量メトリック (高速ティック用)。
/// 履歴サンプルの連続取得に使う。
#[derive(Clone, Debug, Default)]
pub struct FastMetrics {
    pub cpu: f32,
    pub core_usage: Vec<f32>,
    pub mem_pct: f32,
    /// (ディスク ID, アクティブ%, 読取 B/s, 書込 B/s)
    pub disks: Vec<(String, f32, f64, f64)>,
    /// (GPU ID, 使用率)
    pub gpus: Vec<(String, f32)>,
    /// (NPU ID, 使用率)
    pub npus: Vec<(String, f32)>,
    /// GPU エンジン種別ごとの使用率
    pub gpu_engines: Vec<(String, f32)>,
    /// NPU デバイス (LUID) ごとのエンジン別使用率
    pub npu_by_luid: Vec<(String, Vec<(String, f32)>)>,
    /// (アダプター ID, 受信 B/s, 送信 B/s)
    pub adapters: Vec<(String, f64, f64)>,
    pub gpu_dedicated: u64,
    pub npu_dedicated: u64,
}

impl ProcCollector {
    pub fn new() -> Result<Self, String> {
        let mut sys = System::new().map_err(|e| format!("システム情報の初期化に失敗: {e}"))?;
        // ブランド名・周波数も取得しておく
        sys.refresh_cpu_specifics(CpuRefreshKind::everything());
        // PDH 使用率カウンタは初回収集時点では基準値がないため異常値になる。
        // 少し待って 2 度目の収集まで行い、最初の実サンプルが正しくなるようにする。
        std::thread::sleep(std::time::Duration::from_millis(150));
        sys.refresh_cpu_specifics(
            CpuRefreshKind::nothing().with_usage().with_frequency(),
        );
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
            disk_meter: DiskMeter::new(),
            net_meter: NetAdapterMeter::new(),
            gpu_memory_meter: GpuMemoryMeter::new(),
            npu: EngineMeter::new(NPU_ENGINE_COUNTER),
            gpu_engine_meter: EngineMeter::new(GPU_ENGINE_COUNTER),
            npu_device_present: crate::perf::npu_device_present(),
            npu_recheck: 0,
            smart_cache: HashMap::new(),
        })
    }

    /// グラフ描画用の軽量メトリック (高速ティック用)。
    /// プロセステーブル等の重い処理を含まないため高頻度で呼べる。
    pub fn refresh_fast(&mut self) -> FastMetrics {
        // CPU 使用率 (前回呼び出しからの差分で算出される)
        self.sys
            .refresh_cpu_specifics(CpuRefreshKind::nothing().with_usage());
        self.sys.refresh_memory();
        let cpu = self.sys.global_cpu_usage();
        let core_usage: Vec<f32> = self.sys.cpus().iter().map(|c| c.usage()).collect();
        let mem_pct = if self.sys.total_memory() > 0 {
            self.sys.used_memory() as f32 / self.sys.total_memory() as f32 * 100.0
        } else {
            0.0
        };

        let disks: Vec<(String, f32, f64, f64)> = self
            .disk_meter
            .read()
            .into_iter()
            .map(|s| (s.id, s.active_pct, s.read_bps, s.write_bps))
            .collect();

        let adapters: Vec<(String, f64, f64)> = self
            .net_meter
            .read()
            .into_iter()
            .map(|a| (a.id, a.recv_bps as f64, a.sent_bps as f64))
            .collect();

        self.gpus.refresh(false);
        let gpus: Vec<(String, f32)> = self
            .gpus
            .iter()
            .enumerate()
            .map(|(i, g)| (i.to_string(), g.usage().unwrap_or(0.0).clamp(0.0, 100.0)))
            .collect();

        let snapshot = self.gpu_engine_meter.read_split().unwrap_or_default();
        let npus: Vec<(String, f32)> = snapshot
            .npu_by_luid
            .iter()
            .map(|(luid, engines)| {
                let usage = engines.iter().map(|(_, v)| *v).sum::<f32>().clamp(0.0, 100.0);
                (luid.clone(), usage)
            })
            .collect();
        let gpu_mem = self.gpu_memory_meter.read(&snapshot.gpu_luids);

        FastMetrics {
            cpu,
            core_usage,
            mem_pct,
            disks,
            gpus,
            npus,
            gpu_engines: snapshot.gpu_engines,
            npu_by_luid: snapshot.npu_by_luid,
            adapters,
            gpu_dedicated: gpu_mem.gpu_dedicated,
            npu_dedicated: gpu_mem.npu_dedicated,
        }
    }

    /// SMART 情報を 60 秒ごとに更新して返す。
    fn smart_for(&mut self, id: &str) -> Option<Arc<SmartInfo>> {
        if let Some((at, info)) = self.smart_cache.get(id) {
            if at.elapsed() < Duration::from_secs(60) {
                return Some(info.clone());
            }
        }
        let info = Arc::new(smart::query(id));
        self.smart_cache
            .insert(id.to_owned(), (Instant::now(), info.clone()));
        Some(info)
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

        // --- ストレージ: 物理ディスク単位で列挙 (切断時は自動的に消える) ---
        // PDH の PhysicalDisk インスタンスは "1 c:" / "2 d: e:" のように
        // 同じ物理ディスクの複数パーティションを 1 つにまとめる。
        self.disks.refresh(false);
        let disk_samples = self.disk_meter.read();
        let disk_list: Vec<DiskInfo> = if disk_samples.is_empty() {
            // フォールバック: sysinfo のボリューム単位
            self.disks
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let mount = d
                        .mount_point()
                        .to_string_lossy()
                        .trim_end_matches('\\')
                        .to_uppercase();
                    let du = d.usage();
                    DiskInfo {
                        id: i.to_string(),
                        label: format!("ディスク {} ({})", i, mount),
                        letters: vec![mount.trim_end_matches(':').to_owned()],
                        read_bps: (du.read_bytes as f64 / dt) as u64,
                        write_bps: (du.written_bytes as f64 / dt) as u64,
                        active_pct: 0.0,
                        response_ms: 0.0,
                        type_label: "固定ディスク".to_owned(),
                        free: d.available_space(),
                        total: d.total_space(),
                        smart: None,
                    }
                })
                .collect()
        } else {
            disk_samples
                .into_iter()
                .map(|s| {
                    // 容量情報はドライブレターから sysinfo のディスクを引く
                    let cap = s.letters.iter().find_map(|letter| {
                        self.disks.iter().find(|d| {
                            d.mount_point()
                                .to_string_lossy()
                                .to_uppercase()
                                .starts_with(&format!("{letter}:"))
                        })
                    });
                    let type_label = s
                        .letters
                        .first()
                        .map(|letter| crate::perf::disk_type_label(letter))
                        .unwrap_or_else(|| "リムーバブル (USB)".to_owned());
                    let mut info = DiskInfo {
                        id: s.id.clone(),
                        label: s.label,
                        letters: s.letters,
                        read_bps: s.read_bps as u64,
                        write_bps: s.write_bps as u64,
                        active_pct: s.active_pct,
                        response_ms: s.response_ms,
                        type_label,
                        free: cap.map(|d| d.available_space()).unwrap_or(0),
                        total: cap.map(|d| d.total_space()).unwrap_or(0),
                        smart: None,
                    };
                    info.smart = self.smart_for(&s.id);
                    info
                })
                .collect()
        };
        // 切断されたディスクの SMART キャッシュを破棄
        let active_disks: HashSet<String> = disk_list.iter().map(|d| d.id.clone()).collect();
        self.smart_cache.retain(|k, _| active_disks.contains(k));

        // 互換用の容量使用率 (C:)。グラフには使用しない。
        let rom = self
            .disks
            .iter()
            .find(|d| d.mount_point().to_string_lossy().starts_with("C:"))
            .map(|d| {
                let t = d.total_space();
                if t > 0 {
                    ((t - d.available_space().min(t)) as f64 / t as f64 * 100.0) as f32
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);

        // --- GPU: 搭載中の全 GPU を列挙 (非搭載なら空になる) ---
        self.gpus.refresh(false);
        let mut gpu_list: Vec<GpuInfo> = self
            .gpus
            .iter()
            .enumerate()
            .map(|(i, g)| GpuInfo {
                id: i.to_string(),
                label: format!("GPU {i}"),
                model: g.model().unwrap_or("不明").to_owned(),
                usage: g.usage().unwrap_or(0.0).clamp(0.0, 100.0),
                mem_used: g.used_memory().unwrap_or(0),
                mem_total: g.total_memory().unwrap_or(0),
                engines: Vec::new(), // 下でエンジン一覧を設定する
            })
            .collect();
        let primary_gpu_info = gpu_list
            .iter()
            .find(|g| g.usage > 0.0)
            .or_else(|| gpu_list.first())
            .cloned()
            .unwrap_or_default();
        let gpu = primary_gpu_info.usage;

        // --- GPU / NPU エンジン使用率 ---
        // Windows 11 では NPU も \GPU Engine(*) カウンタに含まれ、NPU 固有の
        // LUID で列挙される。3D/Copy/Video 系エンジンを持つ LUID = GPU、
        // compute 系のみの LUID = NPU として分離する (本家と同じ判定)。
        let snapshot = self.gpu_engine_meter.read_split().unwrap_or_default();
        let gpu_engines = snapshot.gpu_engines.clone();
        let gpu_mem = self.gpu_memory_meter.read(&snapshot.gpu_luids);
        for g in &mut gpu_list {
            g.engines = gpu_engines.clone();
        }

        // 専用の \NPU Engine(*) カウンタが存在する環境ではそちらを優先する
        self.npu_recheck = self.npu_recheck.wrapping_add(1);
        if !self.npu.available && self.npu_recheck % 30 == 0 {
            self.npu.try_reopen();
        }
        let npu_snapshot = if self.npu.available {
            self.npu.read_split().unwrap_or_default()
        } else {
            snapshot
        };
        let npu_engines = npu_snapshot.npu_engines;
        let npu: f32 = npu_engines.iter().map(|(_, v)| *v).sum::<f32>().clamp(0.0, 100.0);
        let npu_available = !npu_engines.is_empty();

        // --- ネットワーク アダプター (送受信速度と詳細) ---
        let adapters = self.net_meter.read();

        // NPU は LUID (デバイス) ごとに 1 エントリ。非搭載なら空 → ナビ非表示。
        let mut npu_list: Vec<NpuInfo> = npu_snapshot
            .npu_by_luid
            .into_iter()
            .filter(|(_, engines)| !engines.is_empty())
            .enumerate()
            .map(|(i, (luid, engines))| {
                let usage = engines.iter().map(|(_, v)| *v).sum::<f32>().clamp(0.0, 100.0);
                NpuInfo {
                    id: luid,
                    label: format!("NPU {i}"),
                    name: "NPU Compute Accelerator Device".to_owned(),
                    usage,
                    engines,
                }
            })
            .collect();
        // カウンタ未提供だがデバイスは搭載されている場合: 0% の NPU を表示
        if npu_list.is_empty() && self.npu_device_present {
            npu_list.push(NpuInfo {
                id: "device".to_owned(),
                label: "NPU 0".to_owned(),
                name: "NPU Compute Accelerator Device".to_owned(),
                usage: 0.0,
                engines: Vec::new(),
            });
        }

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
        let core_usage: Vec<f32> = self.sys.cpus().iter().map(|c| c.usage()).collect();

        // --- システム全体の統計 (プロセス/スレッド/ハンドル/コミット) ---
        let (mem_available, commit_used, commit_limit, cached, kernel_paged, kernel_nonpaged, processes, threads, handles) =
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
                        pi.KernelPaged as u64 * ps,
                        pi.KernelNonpaged as u64 * ps,
                        pi.ProcessCount,
                        pi.ThreadCount,
                        pi.HandleCount,
                    )
                } else {
                    (0, 0, 0, 0, 0, 0, 0, 0, 0)
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
            npu_device_present: self.npu_device_present,
            cpu_brand,
            cpu_freq_mhz,
            logical_cores,
            mem_available,
            commit_used,
            commit_limit,
            cached,
            kernel_paged,
            kernel_nonpaged,
            processes,
            threads,
            handles,
            uptime_secs,
            core_usage,
            gpu_engines,
            npu_engines,
            disks: disk_list,
            gpus: gpu_list,
            npus: npu_list,
            adapters,
            gpu_dedicated: gpu_mem.gpu_dedicated,
            gpu_shared: gpu_mem.gpu_shared,
            npu_dedicated: gpu_mem.npu_dedicated,
            npu_shared: gpu_mem.npu_shared,
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
                parent: p.parent().map(|pp| pp.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                status: match p.status() {
                    sysinfo::ProcessStatus::Run => "実行中".to_owned(),
                    sysinfo::ProcessStatus::Sleep => "スリープ".to_owned(),
                    sysinfo::ProcessStatus::Idle => "アイドル".to_owned(),
                    _ => "不明".to_owned(),
                },
                cpu: (p.cpu_usage() / self.cpu_count).clamp(0.0, 100.0),
                mem: p.memory(),
                gpu: p.gpu_usage().unwrap_or(0.0).clamp(0.0, 100.0),
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

/// プロセス ツリー (対象とその子孫すべて) を強制終了する。
/// タスクマネージャーの「プロセス ツリーの終了」相当。
pub fn kill_tree(pid: u32, rows: &[ProcRow]) -> Result<(), String> {
    // 親子関係から子孫を収集
    let mut victims = vec![pid];
    let mut changed = true;
    while changed {
        changed = false;
        for r in rows {
            if let Some(parent) = r.parent {
                if victims.contains(&parent) && !victims.contains(&r.pid) {
                    victims.push(r.pid);
                    changed = true;
                }
            }
        }
    }
    let mut first_err = None;
    for &v in &victims {
        if let Err(e) = kill(v) {
            if v == pid {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
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
