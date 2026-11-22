//! アプリ本体: 状態管理と UI 描画。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    mpsc::{self, Receiver, Sender},
    Arc,
};
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Vec2};
use egui_extras::{Column, TableBuilder};

use crate::icon::{self, IconCache};
use crate::integration;
use crate::net::NetSession;
use crate::process::{self, FastMetrics, ProcCollector, ProcRow, SysLoad};
use crate::settings::AppSettings;
use crate::startup::{self, StartupEntry};
use crate::util::{self, HISTORY_LEN, Palette};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Processes,
    Performance,
    Startup,
}

/// 更新チェック / ダウンロードの結果を UI スレッドへ渡す。
enum UpdateMsg {
    Check(Option<crate::update::UpdateInfo>),
    Download(Result<std::path::PathBuf, String>),
}

/// パフォーマンスタブで選択中のメトリクス。
/// ディスク / GPU / NPU はデバイス ID を持ち、切断時は自動的に別の項目へ切り替わる。
#[derive(Clone, PartialEq, Eq)]
enum PerfSel {
    Cpu,
    Ram,
    /// 空 ID は「最初のディスク」を表す (起動時フック用)
    Disk(String),
    Gpu(String),
    Npu(String),
    /// ネットワーク アダプター (PDH インスタンス名)
    Net(String),
}

/// CPU グラフの表示モード (グラフ種別の切替)。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CpuGraphMode {
    Overall,
    PerCore,
}

/// デバイス履歴に 1 サンプル追加。alpha < 1 のとき EMA 平滑化を行う。
fn push_device_hist(
    hists: &mut Vec<(String, VecDeque<f32>)>,
    id: &str,
    value: f32,
    alpha: f32,
) {
    let hist = match hists.iter_mut().find(|(k, _)| k == id) {
        Some((_, h)) => h,
        None => {
            hists.push((id.to_owned(), VecDeque::new()));
            &mut hists.last_mut().unwrap().1
        }
    };
    let smoothed = smooth_value(hist.back().copied(), value, alpha);
    hist.push_back(smoothed);
    while hist.len() > HISTORY_LEN {
        hist.pop_front();
    }
}

/// EMA 平滑化 (alpha = 1 なら生値)。更新間隔から時定数で alpha を決める。
fn smooth_value(prev: Option<f32>, value: f32, alpha: f32) -> f32 {
    match prev {
        Some(p) if alpha < 1.0 => p + (value - p) * alpha,
        _ => value,
    }
}

/// デバイス履歴を検索 (見つからなければ空の履歴)。
fn find_hist<'a>(
    hists: &'a [(String, VecDeque<f32>)],
    id: &str,
    empty: &'a VecDeque<f32>,
) -> &'a VecDeque<f32> {
    hists
        .iter()
        .find(|(k, _)| k == id)
        .map(|(_, h)| h)
        .unwrap_or(empty)
}

/// プロセス一覧の列。タブの右クリックメニューで表示/非表示を切り替える。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Name,
    Status,
    Pid,
    Cpu,
    Mem,
    NetRecv,
    NetSent,
    IoRead,
    IoWrite,
    Gpu,
    User,
    Start,
    Exe,
    Cmd,
}

impl ColumnKind {
    const ALL: [Self; 14] = [
        Self::Name,
        Self::Status,
        Self::Pid,
        Self::Cpu,
        Self::Mem,
        Self::NetRecv,
        Self::NetSent,
        Self::IoRead,
        Self::IoWrite,
        Self::Gpu,
        Self::User,
        Self::Start,
        Self::Exe,
        Self::Cmd,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Name => "名前",
            Self::Status => "状態",
            Self::Pid => "PID",
            Self::Cpu => "CPU",
            Self::Mem => "メモリ",
            Self::NetRecv => "ネット受信",
            Self::NetSent => "ネット送信",
            Self::IoRead => "I/O 読取",
            Self::IoWrite => "I/O 書込",
            Self::Gpu => "GPU",
            Self::User => "ユーザー",
            Self::Start => "開始時刻",
            Self::Exe => "パス",
            Self::Cmd => "コマンドライン",
        }
    }

    fn is_numeric(self) -> bool {
        !matches!(self, Self::Name | Self::Status | Self::User | Self::Exe | Self::Cmd)
    }

    fn spec(self) -> Column {
        match self {
            Self::Name => Column::auto().at_least(180.0).clip(true),
            Self::Status => Column::initial(80.0).clip(true),
            Self::Pid => Column::initial(60.0).clip(true),
            Self::Cpu => Column::initial(64.0).clip(true),
            Self::Mem => Column::initial(88.0).clip(true),
            Self::NetRecv => Column::initial(92.0).clip(true),
            Self::NetSent => Column::initial(92.0).clip(true),
            Self::IoRead => Column::initial(88.0).clip(true),
            Self::IoWrite => Column::initial(88.0).clip(true),
            Self::Gpu => Column::initial(64.0).clip(true),
            Self::User => Column::initial(110.0).clip(true),
            Self::Start => Column::initial(126.0).clip(true),
            Self::Exe => Column::auto().at_least(220.0).clip(true),
            Self::Cmd => Column::auto().at_least(260.0).clip(true),
        }
    }

    fn render(self, ui: &mut egui::Ui, row: &ProcRow, pal: &Palette) {
        match self {
            Self::Name => {
                ui.label(&row.name);
            }
            Self::Status => {
                let color = if row.status == "実行中" {
                    ui.visuals().weak_text_color()
                } else {
                    pal.warn
                };
                ui.label(RichText::new(&row.status).size(12.0).color(color));
            }
            Self::Pid => util::num_cell(ui, row.pid.to_string(), None),
            Self::Cpu => util::num_cell(ui, format!("{:.1}%", row.cpu), util::cpu_color(row.cpu, pal)),
            Self::Mem => util::num_cell(ui, util::fmt_bytes(row.mem), None),
            Self::NetRecv => util::num_cell(
                ui,
                util::fmt_rate(row.net_recv),
                (row.net_recv > 0).then_some(pal.net_recv),
            ),
            Self::NetSent => util::num_cell(
                ui,
                util::fmt_rate(row.net_sent),
                (row.net_sent > 0).then_some(pal.net_send),
            ),
            Self::IoRead => util::num_cell(ui, util::fmt_rate(row.io_read), None),
            Self::IoWrite => util::num_cell(ui, util::fmt_rate(row.io_write), None),
            Self::Gpu => util::num_cell(
                ui,
                if row.gpu > 0.0 { format!("{:.1}%", row.gpu) } else { "—".to_owned() },
                util::cpu_color(row.gpu, pal),
            ),
            Self::User => {
                ui.label(&row.user);
            }
            Self::Start => {
                ui.label(RichText::new(util::fmt_time(row.start_time)).monospace().size(12.0));
            }
            Self::Exe => {
                ui.label(RichText::new(&row.exe).size(12.0));
            }
            Self::Cmd => {
                ui.label(RichText::new(&row.cmd).size(12.0));
            }
        }
    }
}

pub struct MiniTaskApp {
    collector: Option<ProcCollector>,
    rows: Vec<ProcRow>,
    load: SysLoad,
    cpu_hist: VecDeque<f32>,
    mem_hist: VecDeque<f32>,
    /// 物理ディスクごとの読み取り/書き込み履歴 (切断時に自動で破棄)
    disk_active_hist: Vec<(String, VecDeque<f32>)>,
    disk_read_hist: Vec<(String, VecDeque<f32>)>,
    disk_write_hist: Vec<(String, VecDeque<f32>)>,
    gpu_hist: Vec<(String, VecDeque<f32>)>,
    npu_hist: Vec<(String, VecDeque<f32>)>,
    /// ネットワークアダプターごとの受信/送信履歴
    net_recv_hist: Vec<(String, VecDeque<f32>)>,
    net_sent_hist: Vec<(String, VecDeque<f32>)>,
    /// GPU/NPU 専用メモリの履歴 (第2グラフ用)
    gpu_mem_hist: VecDeque<f32>,
    npu_mem_hist: VecDeque<f32>,
    net: Option<NetSession>,
    net_err: Option<String>,

    tab: Tab,
    perf_sel: PerfSel,
    cpu_graph_mode: CpuGraphMode,
    core_hist: Vec<VecDeque<f32>>,
    /// GPU エンジン種別ごとの履歴 (キーは engtype の小文字)
    gpu_engine_hist: Vec<(String, VecDeque<f32>)>,
    /// NPU エンジン種別ごとの履歴
    npu_engine_hist: Vec<(String, VecDeque<f32>)>,
    /// GPU 大グラフに表示する種別 (None = 全体的な使用率)
    gpu_sel: Option<String>,
    /// NPU 大グラフに表示する種別
    npu_sel: Option<String>,
    search: String,
    sort_col: ColumnKind,
    sort_desc: bool,
    selected: Option<u32>,
    columns: [bool; 14],
    interval: f32,
    /// 「更新の速度」メニューによる一時停止中
    paused: bool,
    /// グラフのなめらかなアニメーション補間 (更新の速度メニューから切替)
    graph_animation: bool,
    /// 折れ線(false) / スプライン補間(true)
    graph_spline: bool,
    /// 高速サンプリング (アニメーション補間用) の最終実行時刻
    last_hist_tick: Instant,
    /// パフォーマンス右側の詳細・余白の実測高さ (毎フレーム自動補正)
    perf_overhead: f32,
    last_refresh: Instant,

    confirm_kill: Option<(u32, String)>,
    confirm_kill_tree: Option<(u32, String)>,
    toast: Option<(String, Instant)>,

    startup_entries: Vec<StartupEntry>,
    confirm_delete: Option<StartupEntry>,
    /// 最後に保存した設定 (差分検出用)
    saved_settings: AppSettings,

    // --- 外観・動作 ---
    palette: Palette,
    dark_mode: bool,
    theme_dirty: bool,
    always_on_top: bool,
    startup_tab_mode: u8,
    confirm_kill_setting: bool,
    settings_open: bool,
    settings_page: u8,

    // --- システム統合 ---
    replace_taskmgr: bool,
    tray_icon: bool,
    close_to_tray: bool,
    minimize_to_tray: bool,
    minimize_flag: Arc<AtomicBool>,
    tray_usage_tip: bool,
    last_tray_tip: String,
    last_tray_check: Instant,
    tray_handle: Option<crate::tray::TrayHandle>,
    tray_rx: Option<Receiver<crate::tray::TrayMsg>>,
    ifeo_restore: Option<(String, Instant)>,
    taskmgr_icon: Option<egui::TextureHandle>,

    // --- アップデート ---
    update_rx: Receiver<UpdateMsg>,
    update_tx: Sender<UpdateMsg>,
    update_info: Option<crate::update::UpdateInfo>,
    update_busy: bool,
    update_msg: Option<String>,
    last_update_check: u64,

    // --- アイコン / SMART ---
    icon_cache: IconCache,
    smart_dialog_disk: Option<String>,
}

/// グラフ履歴へのサンプル反映に必要な値のまとまり。
/// 通常リフレッシュ (アニメーションOFF) と高速ティック (ON) の両方で使う。
struct PerfSample {
    cpu: f32,
    core_usage: Vec<f32>,
    mem_pct: f32,
    disks: Vec<(String, f32, f64, f64)>,
    gpus: Vec<(String, f32)>,
    npus: Vec<(String, f32)>,
    gpu_engines: Vec<(String, f32)>,
    /// (NPU LUID, エンジン別使用率)
    npu_by_luid: Vec<(String, Vec<(String, f32)>)>,
    adapters: Vec<(String, f64, f64)>,
    gpu_dedicated: u64,
    npu_dedicated: u64,
}

impl PerfSample {
    fn from_load(load: &SysLoad) -> Self {
        Self {
            cpu: load.cpu,
            core_usage: load.core_usage.clone(),
            mem_pct: if load.mem_total > 0 {
                load.mem_used as f32 / load.mem_total as f32 * 100.0
            } else {
                0.0
            },
            disks: load
                .disks
                .iter()
                .map(|d| {
                    (
                        d.id.clone(),
                        d.active_pct,
                        d.read_bps as f64,
                        d.write_bps as f64,
                    )
                })
                .collect(),
            gpus: load.gpus.iter().map(|g| (g.id.clone(), g.usage)).collect(),
            npus: load.npus.iter().map(|n| (n.id.clone(), n.usage)).collect(),
            gpu_engines: load.gpu_engines.clone(),
            npu_by_luid: load
                .npus
                .iter()
                .map(|n| (n.id.clone(), n.engines.clone()))
                .collect(),
            adapters: load
                .adapters
                .iter()
                .map(|a| (a.id.clone(), a.recv_bps as f64, a.sent_bps as f64))
                .collect(),
            gpu_dedicated: load.gpu_dedicated,
            npu_dedicated: load.npu_dedicated,
        }
    }

    fn from_fast(m: &FastMetrics) -> Self {
        Self {
            cpu: m.cpu,
            core_usage: m.core_usage.clone(),
            mem_pct: m.mem_pct,
            disks: m.disks.clone(),
            gpus: m.gpus.clone(),
            npus: m.npus.clone(),
            gpu_engines: m.gpu_engines.clone(),
            npu_by_luid: m.npu_by_luid.clone(),
            adapters: m.adapters.clone(),
            gpu_dedicated: m.gpu_dedicated,
            npu_dedicated: m.npu_dedicated,
        }
    }
}

impl MiniTaskApp {
    pub fn new(ctx: &egui::Context) -> Self {
        let settings = AppSettings::load();
        // 最小化中は egui の update が止まるため、別スレッドで最小化を監視して
        // トレイへ格納する (フラグは設定画面から即時に反映される)。
        let minimize_flag = Arc::new(AtomicBool::new(
            settings.minimize_to_tray && settings.tray_icon,
        ));
        {
            let flag = Arc::clone(&minimize_flag);
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                if flag.load(Ordering::Relaxed) {
                    integration::try_hide_minimized_window();
                }
            });
        }
        // GitHub Releases を 1 日 1 回チェック (結果はチャネルで受け取る)
        let (update_tx, update_rx) = mpsc::channel::<UpdateMsg>();
        if crate::update::now_secs().saturating_sub(settings.last_update_check)
            >= crate::update::CHECK_INTERVAL_SECS
        {
            let tx = update_tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(UpdateMsg::Check(crate::update::check_latest()));
            });
        }
        let env_tab = std::env::var("MINITASK_TAB").ok();
        let collector = ProcCollector::new().ok();

        // システム統合の適用 (権限不足で失敗しても起動は続ける)
        if let Ok(exe) = std::env::current_exe() {
            let exe = exe.to_string_lossy().to_string();
            if settings.replace_taskmgr {
                let _ = integration::set_replace_taskmgr(true, &exe);
            }
            if settings.tray_icon {
                let _ = integration::set_shell_menu(true, &exe);
            }
        }
        let (tray_handle, tray_rx) = if settings.tray_icon {
            let (h, rx) = crate::tray::start();
            (Some(h), Some(rx))
        } else {
            (None, None)
        };
        let taskmgr_icon = {
            let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
            icon::extract_icon(&format!("{windir}\\System32\\taskmgr.exe"), true).map(
                |(w, h, rgba)| {
                    ctx.load_texture(
                        "taskmgr_icon",
                        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba),
                        egui::TextureOptions::LINEAR,
                    )
                },
            )
        };
        let (net, net_err) = if std::env::var("MINITASK_NO_ETW").is_ok() {
            (None, Some("MINITASK_NO_ETW により無効化".to_owned()))
        } else {
            match NetSession::start() {
                Ok(session) => (Some(session), None),
                Err(e) => (None, Some(e)),
            }
        };
        let mut app = Self {
            collector,
            rows: Vec::new(),
            load: SysLoad::default(),
            cpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
            disk_active_hist: Vec::new(),
            disk_read_hist: Vec::new(),
            disk_write_hist: Vec::new(),
            gpu_hist: Vec::new(),
            npu_hist: Vec::new(),
            net_recv_hist: Vec::new(),
            net_sent_hist: Vec::new(),
            gpu_mem_hist: VecDeque::new(),
            npu_mem_hist: VecDeque::new(),
            net,
            net_err,
            // 環境変数 (検証用フック) は保存済み設定より優先する
            tab: match env_tab.as_deref() {
                Some("processes") => Tab::Processes,
                Some(_) => Tab::Performance,
                None => match settings.initial_tab() {
                    1 => Tab::Performance,
                    2 => Tab::Startup,
                    _ => Tab::Processes,
                },
            },
            perf_sel: match env_tab.as_deref() {
                Some("ram") | Some("memory") => PerfSel::Ram,
                Some("rom") | Some("disk") => PerfSel::Disk(String::new()),
                Some("gpu") => PerfSel::Gpu(String::new()),
                Some("npu") => PerfSel::Npu(String::new()),
                Some("net") | Some("network") => PerfSel::Net(String::new()),
                Some(_) => PerfSel::Cpu,
                None => match settings.perf_kind {
                    1 => PerfSel::Ram,
                    2 => PerfSel::Disk(settings.perf_id.clone()),
                    3 => PerfSel::Gpu(settings.perf_id.clone()),
                    4 => PerfSel::Npu(settings.perf_id.clone()),
                    5 => PerfSel::Net(settings.perf_id.clone()),
                    _ => PerfSel::Cpu,
                },
            },
            cpu_graph_mode: match env_tab.as_deref() {
                Some("percore") | Some("logical") => CpuGraphMode::PerCore,
                Some(_) => CpuGraphMode::Overall,
                None => {
                    if settings.cpu_graph_mode {
                        CpuGraphMode::PerCore
                    } else {
                        CpuGraphMode::Overall
                    }
                }
            },
            core_hist: Vec::new(),
            gpu_engine_hist: Vec::new(),
            npu_engine_hist: Vec::new(),
            gpu_sel: None,
            npu_sel: None,
            search: String::new(),
            sort_col: ColumnKind::ALL
                .get(settings.sort_col as usize)
                .copied()
                .unwrap_or(ColumnKind::Cpu),
            sort_desc: settings.sort_desc,
            selected: None,
            // 列表示は保存済みビットマスクから復元 (既定はよく使う7列)
            columns: {
                let mut cols = [false; 14];
                for (i, on) in cols.iter_mut().enumerate() {
                    *on = settings.columns_mask & (1 << i) != 0;
                }
                cols[0] = true; // 名前列は常に表示
                cols
            },
            interval: settings.interval_ms as f32 / 1000.0,
            paused: false,
            graph_animation: if std::env::var("MINITASK_ANIMATION").is_ok() {
                true
            } else {
                settings.graph_animation
            },
            graph_spline: if std::env::var("MINITASK_SPLINE").is_ok() {
                true
            } else {
                settings.graph_spline
            },
            last_hist_tick: Instant::now() - Duration::from_secs(10),
            perf_overhead: 360.0,
            last_refresh: Instant::now() - Duration::from_secs(10),
            confirm_kill: None,
            confirm_kill_tree: None,
            toast: None,
            startup_entries: startup::list(),
            confirm_delete: None,
            saved_settings: settings.snapshot(),
            palette: settings.active_palette(),
            dark_mode: settings.dark_mode,
            theme_dirty: true,
            always_on_top: settings.always_on_top,
            startup_tab_mode: settings.startup_tab_mode,
            confirm_kill_setting: settings.confirm_kill,
            settings_open: false,
            settings_page: 0,
            replace_taskmgr: settings.replace_taskmgr,
            tray_icon: settings.tray_icon,
            close_to_tray: settings.close_to_tray,
            minimize_to_tray: settings.minimize_to_tray,
            minimize_flag,
            tray_usage_tip: settings.tray_usage_tip,
            last_tray_tip: String::new(),
            last_tray_check: Instant::now() - Duration::from_secs(1),
            tray_handle,
            tray_rx,
            ifeo_restore: None,
            taskmgr_icon,
            update_rx,
            update_tx,
            update_info: None,
            update_busy: false,
            update_msg: None,
            last_update_check: settings.last_update_check,
            icon_cache: IconCache::default(),
            smart_dialog_disk: None,
        };
        app.refresh();
        app
    }

    /// 指定ディスクの SMART をキャッシュ無視で再取得し、表示データに反映する。
    fn refresh_smart(&mut self, id: &str) {
        let Some(info) = self.collector.as_mut().and_then(|c| c.refresh_smart(id)) else {
            return;
        };
        if let Some(disk) = self.load.disks.iter_mut().find(|d| d.id == id) {
            disk.smart = Some(info);
        }
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if let Some(collector) = &mut self.collector {
            let (rows, load) = collector.refresh();
            self.rows = rows;
            // アニメーションOFF時のみここで履歴へ反映。
            // ON時は fast_tick() が 250ms ごとにシームレスに反映する。
            if !self.graph_animation {
                let sample = PerfSample::from_load(&load);
                self.push_history_samples(&sample, 1.0);
            }

            self.load = load;
            self.normalize_selection();
            if let Some(pid) = self.selected {
                if !self.rows.iter().any(|r| r.pid == pid) {
                    self.selected = None;
                }
            }
        }
        if let Some((_, t)) = &self.toast {
            if t.elapsed() > Duration::from_secs(5) {
                self.toast = None;
            }
        }
    }

    /// アニメーション補間ON時の高速サンプリング (250ms ごと)。
    /// 更新間隔と無関係に履歴サンプルが流れ込み、線がシームレスに伸びる。
    fn fast_tick(&mut self) {
        self.last_hist_tick = Instant::now();
        let Some(collector) = &mut self.collector else {
            return;
        };
        let metrics = collector.refresh_fast();
        let sample = PerfSample::from_fast(&metrics);
        // 時定数 2.5 秒、サンプリング間隔 250ms の EMA 平滑化
        let alpha = (1.0 - (-0.25_f32 / 2.5).exp()).clamp(0.01, 1.0);
        self.push_history_samples(&sample, alpha);
    }

    /// 全履歴系列へ 1 サンプル反映し、切断されたデバイスの履歴を破棄する。
    fn push_history_samples(&mut self, s: &PerfSample, alpha: f32) {
        self.cpu_hist
            .push_back(smooth_value(self.cpu_hist.back().copied(), s.cpu, alpha));
        self.mem_hist
            .push_back(smooth_value(self.mem_hist.back().copied(), s.mem_pct, alpha));

        // 接続中のデバイスごとに履歴を更新
        for (id, active, read, write) in &s.disks {
            push_device_hist(&mut self.disk_active_hist, id, *active, alpha);
            push_device_hist(&mut self.disk_read_hist, id, *read as f32, alpha);
            push_device_hist(&mut self.disk_write_hist, id, *write as f32, alpha);
        }
        for (id, usage) in &s.gpus {
            push_device_hist(&mut self.gpu_hist, id, *usage, alpha);
        }
        for (id, usage) in &s.npus {
            push_device_hist(&mut self.npu_hist, id, *usage, alpha);
        }
        for (id, recv, sent) in &s.adapters {
            push_device_hist(&mut self.net_recv_hist, id, *recv as f32, alpha);
            push_device_hist(&mut self.net_sent_hist, id, *sent as f32, alpha);
        }
        // GPU/NPU の専用メモリ履歴 (第2グラフ)
        let v = smooth_value(
            self.gpu_mem_hist.back().copied(),
            s.gpu_dedicated as f32,
            alpha,
        );
        self.gpu_mem_hist.push_back(v);
        while self.gpu_mem_hist.len() > HISTORY_LEN {
            self.gpu_mem_hist.pop_front();
        }
        let v = smooth_value(
            self.npu_mem_hist.back().copied(),
            s.npu_dedicated as f32,
            alpha,
        );
        self.npu_mem_hist.push_back(v);
        while self.npu_mem_hist.len() > HISTORY_LEN {
            self.npu_mem_hist.pop_front();
        }
        // 切断されたデバイスの履歴を破棄 (再接続時は新しい履歴から開始)
        let disk_ids: HashSet<&str> = s.disks.iter().map(|d| d.0.as_str()).collect();
        let gpu_ids: HashSet<&str> = s.gpus.iter().map(|g| g.0.as_str()).collect();
        let npu_ids: HashSet<&str> = s.npus.iter().map(|n| n.0.as_str()).collect();
        let adapter_ids: HashSet<&str> = s.adapters.iter().map(|a| a.0.as_str()).collect();
        self.net_recv_hist
            .retain(|(id, _)| adapter_ids.contains(id.as_str()));
        self.net_sent_hist
            .retain(|(id, _)| adapter_ids.contains(id.as_str()));
        self.disk_active_hist
            .retain(|(id, _)| disk_ids.contains(id.as_str()));
        self.disk_read_hist
            .retain(|(id, _)| disk_ids.contains(id.as_str()));
        self.disk_write_hist
            .retain(|(id, _)| disk_ids.contains(id.as_str()));
        self.gpu_hist.retain(|(id, _)| gpu_ids.contains(id.as_str()));
        self.npu_hist.retain(|(id, _)| npu_ids.contains(id.as_str()));

        // エンジン種別ごとの履歴 (GPU/NPU の「グラフの変更」用)
        for (key, val) in s.gpu_engines.iter() {
            push_device_hist(&mut self.gpu_engine_hist, key, *val, alpha);
        }
        let gpu_engine_keys: HashSet<&str> =
            s.gpu_engines.iter().map(|(k, _)| k.as_str()).collect();
        self.gpu_engine_hist
            .retain(|(k, _)| gpu_engine_keys.contains(k.as_str()));
        // NPU はデバイス (LUID) ごとにエンジン履歴を分ける: キーは "LUID|engtype"
        for (luid, engines) in &s.npu_by_luid {
            for (key, val) in engines {
                let hist_key = format!("{luid}|{key}");
                push_device_hist(&mut self.npu_engine_hist, &hist_key, *val, alpha);
            }
        }
        let npu_engine_keys: HashSet<String> = s
            .npu_by_luid
            .iter()
            .flat_map(|(luid, engines)| {
                let luid = luid.clone();
                engines.iter().map(move |(k, _)| format!("{luid}|{k}"))
            })
            .collect();
        self.npu_engine_hist
            .retain(|(k, _)| npu_engine_keys.contains(k));

        // コア別履歴
        if self.core_hist.len() != s.core_usage.len() {
            self.core_hist = vec![VecDeque::new(); s.core_usage.len()];
        }
        for (hist, val) in self.core_hist.iter_mut().zip(s.core_usage.iter()) {
            let smoothed = smooth_value(hist.back().copied(), *val, alpha);
            hist.push_back(smoothed);
            while hist.len() > HISTORY_LEN {
                hist.pop_front();
            }
        }
        for hist in [&mut self.cpu_hist, &mut self.mem_hist] {
            while hist.len() > HISTORY_LEN {
                hist.pop_front();
            }
        }
    }

    /// デバイスの増減に合わせて選択中項目を調整する。
    /// 切断されたデバイスが選択中なら別のデバイス (無ければ CPU) に切り替える。
    fn normalize_selection(&mut self) {
        let valid = match &self.perf_sel {
            PerfSel::Cpu | PerfSel::Ram => true,
            PerfSel::Disk(id) => id.is_empty() || self.load.disks.iter().any(|d| &d.id == id),
            PerfSel::Gpu(id) => id.is_empty() || self.load.gpus.iter().any(|g| &g.id == id),
            PerfSel::Npu(id) => id.is_empty() || self.load.npus.iter().any(|n| &n.id == id),
            PerfSel::Net(id) => id.is_empty() || self.load.adapters.iter().any(|a| &a.id == id),
        };
        if !valid {
            self.perf_sel = if let Some(d) = self.load.disks.first() {
                PerfSel::Disk(d.id.clone())
            } else if let Some(g) = self.load.gpus.first() {
                PerfSel::Gpu(g.id.clone())
            } else if let Some(n) = self.load.npus.first() {
                PerfSel::Npu(n.id.clone())
            } else if let Some(a) = self.load.adapters.first() {
                PerfSel::Net(a.id.clone())
            } else {
                PerfSel::Cpu
            };
            self.gpu_sel = None;
            self.npu_sel = None;
        }
        // 起動時フックの空 ID を実 ID に解決
        match &mut self.perf_sel {
            PerfSel::Disk(id) if id.is_empty() => {
                if let Some(d) = self.load.disks.first() {
                    *id = d.id.clone();
                }
            }
            PerfSel::Gpu(id) if id.is_empty() => {
                if let Some(g) = self.load.gpus.first() {
                    *id = g.id.clone();
                }
            }
            PerfSel::Npu(id) if id.is_empty() => {
                if let Some(n) = self.load.npus.first() {
                    *id = n.id.clone();
                }
            }
            PerfSel::Net(id) if id.is_empty() => {
                if let Some(a) = self.load.adapters.first() {
                    *id = a.id.clone();
                }
            }
            _ => {}
        }
        // 選択中のエンジンが現在のデバイスに無ければ解除
        if let Some(key) = &self.gpu_sel {
            if !self.load.gpu_engines.iter().any(|(k, _)| k == key) {
                self.gpu_sel = None;
            }
        }
        if let PerfSel::Npu(id) = &self.perf_sel {
            let has = self
                .load
                .npus
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.engines.iter().any(|(k, _)| k == self.npu_sel.as_deref().unwrap_or("")))
                .unwrap_or(false);
            if !has {
                self.npu_sel = None;
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn ask_kill(&mut self, pid: u32) {
        if let Some(row) = self.rows.iter().find(|r| r.pid == pid) {
            if !self.confirm_kill_setting {
                match process::kill(pid) {
                    Ok(()) => self.notify(format!("PID {pid} を終了しました")),
                    Err(e) => self.notify(e),
                }
                self.selected = None;
                self.refresh();
            } else {
                self.confirm_kill = Some((pid, row.name.clone()));
            }
        }
    }

    fn sorted_view(&self) -> Vec<ProcRow> {
        let q = self.search.trim().to_lowercase();
        let mut view: Vec<ProcRow> = self
            .rows
            .iter()
            .filter(|r| {
                q.is_empty()
                    || r.name.to_lowercase().contains(&q)
                    || r.pid.to_string().contains(&q)
                    || r.exe.to_lowercase().contains(&q)
                    || r.user.to_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        let col = self.sort_col;
        view.sort_by(|a, b| {
            let ord = match col {
                ColumnKind::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                ColumnKind::Status => a.status.cmp(&b.status),
                ColumnKind::Pid => a.pid.cmp(&b.pid),
                ColumnKind::Cpu => a.cpu.total_cmp(&b.cpu),
                ColumnKind::Mem => a.mem.cmp(&b.mem),
                ColumnKind::NetRecv => a.net_recv.cmp(&b.net_recv),
                ColumnKind::NetSent => a.net_sent.cmp(&b.net_sent),
                ColumnKind::IoRead => a.io_read.cmp(&b.io_read),
                ColumnKind::IoWrite => a.io_write.cmp(&b.io_write),
                ColumnKind::Gpu => a.gpu.total_cmp(&b.gpu),
                ColumnKind::User => a.user.to_lowercase().cmp(&b.user.to_lowercase()),
                ColumnKind::Start => a.start_time.cmp(&b.start_time),
                ColumnKind::Exe => a.exe.to_lowercase().cmp(&b.exe.to_lowercase()),
                ColumnKind::Cmd => a.cmd.to_lowercase().cmp(&b.cmd.to_lowercase()),
            };
            if self.sort_desc {
                ord.reverse()
            } else {
                ord
            }
        });
        view
    }

    fn sort_header(&mut self, ui: &mut egui::Ui, col: ColumnKind) {
        let arrow = if self.sort_col == col {
            if self.sort_desc {
                " ▼"
            } else {
                " ▲"
            }
        } else {
            ""
        };
        let text = RichText::new(format!("{}{arrow}", col.label())).strong();
        let resp = ui.button(text);
        if resp.clicked() {
            if self.sort_col == col {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort_col = col;
                self.sort_desc = col.is_numeric();
            }
        }
        // 列見出しの右クリックでも列選択メニューを開く
        resp.context_menu(|ui| self.column_menu(ui));
    }
}

impl eframe::App for MiniTaskApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.theme_dirty {
            util::setup_theme(ctx, self.dark_mode, &self.palette);
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                if self.always_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                },
            ));
            self.theme_dirty = false;
        }
        // トレイアイコンからのメッセージ
        if let Some(rx) = &self.tray_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    crate::tray::TrayMsg::Open => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        integration::focus_window();
                    }
                    crate::tray::TrayMsg::Exit => {
                        // トレイ格納中でも確実に終了させる
                        self.close_to_tray = false;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
        // GitHub Releases の更新チェック / ダウンロード結果
        while let Ok(msg) = self.update_rx.try_recv() {
            match msg {
                UpdateMsg::Check(info) => {
                    self.update_busy = false;
                    self.last_update_check = crate::update::now_secs();
                    match info {
                        Some(i)
                            if crate::update::is_newer(&i.version, env!("CARGO_PKG_VERSION")) =>
                        {
                            self.update_msg = Some(format!("{} が利用可能", i.version));
                            self.toast = Some((
                                format!("MiniTask {} が利用可能", i.version),
                                Instant::now(),
                            ));
                            self.update_info = Some(i);
                        }
                        Some(_) => {
                            self.update_info = None;
                            self.update_msg = Some("最新バージョンです".to_owned());
                        }
                        None => {
                            self.update_msg = Some("更新情報を取得できませんでした".to_owned());
                        }
                    }
                }
                UpdateMsg::Download(Ok(path)) => {
                    self.update_busy = false;
                    if crate::update::apply_update(&path).is_ok() {
                        // デタッチしたヘルパーがインストール→再起動を行う。
                        // アプリは設定を保存してから正常終了する。
                        self.close_to_tray = false;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        self.update_msg = Some("更新の適用に失敗しました".to_owned());
                    }
                }
                UpdateMsg::Download(Err(e)) => {
                    self.update_busy = false;
                    self.update_msg = Some(format!("ダウンロード失敗: {e}"));
                }
            }
        }
        // 閉じるボタンでトレイに格納
        if self.tray_icon && self.close_to_tray && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // トレイのツールチップ更新と、最小化→トレイ格納 (0.5 秒ごとに判定)
        if self.tray_icon && self.last_tray_check.elapsed() >= Duration::from_millis(500) {
            self.last_tray_check = Instant::now();
            if let Some(handle) = self.tray_handle.as_ref() {
                let tip = if self.tray_usage_tip {
                    let mem_pct = if self.load.mem_total > 0 {
                        self.load.mem_used as f32 / self.load.mem_total as f32 * 100.0
                    } else {
                        0.0
                    };
                    format!(
                        "MiniTask\nCPU {:.0}%   メモリ {:.0}%",
                        self.load.cpu, mem_pct
                    )
                } else {
                    "MiniTask — 軽量タスクマネージャー".to_owned()
                };
                if tip != self.last_tray_tip {
                    crate::tray::set_tip(handle.hwnd(), &tip);
                    self.last_tray_tip = tip;
                }
            }
        }
        // 起動置換の一時解除後の IFEO 復元
        if let Some((_, at)) = &self.ifeo_restore {
            if at.elapsed() >= Duration::from_millis(500) {
                if let Some((old, _)) = self.ifeo_restore.take() {
                    integration::restore_ifeo(Some(old));
                }
            }
        }
        let interval = Duration::from_secs_f32(self.interval);
        let elapsed = self.last_refresh.elapsed();
        if elapsed >= interval && !self.paused {
            self.refresh();
        }
        if self.graph_animation && !self.paused {
            // グラフは更新間隔と無関係に 250ms ごとに連続サンプリングし、
            // 線がシームレスに伸びていくようにする
            const FAST_TICK: Duration = Duration::from_millis(250);
            if self.last_hist_tick.elapsed() >= FAST_TICK {
                self.fast_tick();
            }
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(interval.saturating_sub(elapsed));
        }
        // 設定が変わっていれば保存 (差分検出なので各操作側の変更は不要)
        let current = self.current_settings();
        if !current.same(&self.saved_settings) {
            current.save();
            self.saved_settings = current;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // モーダル表示中は背面をグレー化して操作できないようにする
        let modal = self.any_modal();
        ui.scope(|ui| {
            if modal {
                ui.disable();
            }
            egui::Panel::bottom("status_bar")
                .exact_size(30.0)
                .show(ui, |ui| {
                    ui.add_space(2.0);
                    self.status_bar(ui);
                });

            egui::CentralPanel::default().show(ui, |ui| {
                self.header(ui);
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Processes, "  プロセス  ");
                    ui.selectable_value(&mut self.tab, Tab::Performance, "  パフォーマンス  ");
                    ui.selectable_value(&mut self.tab, Tab::Startup, "  スタートアップ  ");
                });
                ui.separator();
                match self.tab {
                    Tab::Processes => self.processes_tab(ui),
                    Tab::Performance => self.performance_tab(ui),
                    Tab::Startup => self.startup_tab(ui),
                }
            });
        });

        self.dialogs(ui.ctx());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.current_settings().save();
        if let Some(mut tray) = self.tray_handle.take() {
            tray.stop();
        }
        if let Some((old, _)) = self.ifeo_restore.take() {
            integration::restore_ifeo(Some(old));
        }
        if let Some(mut session) = self.net.take() {
            session.stop();
        }
    }
}

impl MiniTaskApp {
    /// 現在の設定を保存用の形にまとめる。
    fn current_settings(&self) -> AppSettings {
        let (perf_kind, perf_id) = match &self.perf_sel {
            PerfSel::Cpu => (0, String::new()),
            PerfSel::Ram => (1, String::new()),
            PerfSel::Disk(id) => (2, id.clone()),
            PerfSel::Gpu(id) => (3, id.clone()),
            PerfSel::Npu(id) => (4, id.clone()),
            PerfSel::Net(id) => (5, id.clone()),
        };
        let mut columns_mask = 0u32;
        for (i, on) in self.columns.iter().enumerate() {
            if *on {
                columns_mask |= 1 << i;
            }
        }
        let sort_col = ColumnKind::ALL
            .iter()
            .position(|c| *c == self.sort_col)
            .unwrap_or(3) as u8;
        AppSettings {
            interval_ms: (self.interval * 1000.0) as u32,
            graph_animation: self.graph_animation,
            graph_spline: self.graph_spline,
            columns_mask,
            sort_col,
            sort_desc: self.sort_desc,
            tab: match self.tab {
                Tab::Processes => 0,
                Tab::Performance => 1,
                Tab::Startup => 2,
            },
            perf_kind,
            perf_id,
            cpu_graph_mode: self.cpu_graph_mode == CpuGraphMode::PerCore,
            dark_mode: self.dark_mode,
            dark_colors: if self.dark_mode {
                self.palette
            } else {
                self.saved_settings.dark_colors
            },
            light_colors: if self.dark_mode {
                self.saved_settings.light_colors
            } else {
                self.palette
            },
            always_on_top: self.always_on_top,
            startup_tab_mode: self.startup_tab_mode,
            confirm_kill: self.confirm_kill_setting,
            replace_taskmgr: self.replace_taskmgr,
            tray_icon: self.tray_icon,
            close_to_tray: self.close_to_tray,
            minimize_to_tray: self.minimize_to_tray,
            tray_usage_tip: self.tray_usage_tip,
            last_update_check: self.last_update_check,
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("MiniTask")
                    .size(20.0)
                    .strong()
                    .color(self.palette.primary),
            );
            ui.label(
                RichText::new("軽量タスクマネージャー")
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(info) = &self.update_info {
                    let label = format!("更新あり ({})", info.version);
                    if ui
                        .button(RichText::new(label).color(self.palette.warn))
                        .on_hover_text("クリックして設定から更新できます")
                        .clicked()
                    {
                        self.settings_open = true;
                        self.settings_page = 3;
                    }
                }
                if ui.button("設定").clicked() {
                    self.settings_open = true;
                }
                if self.replace_taskmgr {
                    let button = if let Some(tex) = &self.taskmgr_icon {
                        egui::Button::image(
                            egui::Image::from_texture(tex).fit_to_exact_size(Vec2::splat(18.0)),
                        )
                    } else {
                        egui::Button::new("TM")
                    };
                    if ui
                        .add(button)
                        .on_hover_text("タスクマネージャーを開く")
                        .clicked()
                    {
                        match integration::pause_ifeo_for_taskmgr() {
                            Ok(old) => {
                                if let Some(v) = old {
                                    self.ifeo_restore = Some((v, Instant::now()));
                                }
                            }
                            Err(e) => self.notify(e),
                        }
                    }
                }
            });
        });
    }

    /// プロセスタブの右クリックメニュー: 表示する列を選ぶ。
    fn column_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(180.0);
        ui.label(RichText::new("表示する列").strong());
        ui.separator();
        for (i, c) in ColumnKind::ALL.into_iter().enumerate() {
            if ui.checkbox(&mut self.columns[i], c.label()).clicked() {
                // 何も表示されなくならないよう、名前列は常に表示する
                self.columns[0] = true;
            }
        }
    }

    fn processes_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("検索:");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("名前 / PID / パス / ユーザー")
                    .desired_width(230.0),
            );
            if ui.button("✕").on_hover_text("検索をクリア").clicked() {
                self.search.clear();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(self.selected.is_some(), egui::Button::new("タスクの終了"))
                    .clicked()
                {
                    if let Some(pid) = self.selected {
                        self.ask_kill(pid);
                    }
                }
                if ui.button("更新").clicked() {
                    self.refresh();
                }
            });
        });
        ui.add_space(2.0);

        let view = self.sorted_view();
        self.icon_cache.reset_frame();
        let pal = self.palette;
        // 名前列のアイコンを先に取得 (テーブル描画中の借用を避ける)
        let mut icons: HashMap<u32, Option<egui::TextureHandle>> = HashMap::new();
        for r in view.iter().take(60) {
            let key = if r.exe.is_empty() {
                r.name.clone()
            } else {
                r.exe.clone()
            };
            icons.insert(r.pid, self.icon_cache.get(ui.ctx(), &key, &key));
        }
        // タブの右クリックメニューで選んだ列だけを表示 (名前は常に表示)
        let visible: Vec<ColumnKind> = ColumnKind::ALL
            .into_iter()
            .zip(self.columns)
            .filter_map(|(c, on)| on.then_some(c))
            .collect();

        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        for &c in &visible {
            table = table.column(c.spec());
        }

        table
            .header(24.0, |mut header| {
                for &c in &visible {
                    header.col(|ui| {
                        self.sort_header(ui, c);
                    });
                }
            })
            .body(|mut body| {
                for row in view {
                    body.row(20.0, |mut table_row| {
                        table_row.set_selected(self.selected == Some(row.pid));
                        for &c in &visible {
                            table_row.col(|ui| {
                                if c == ColumnKind::Name {
                                    if let Some(tex) = icons.get(&row.pid).and_then(|t| t.as_ref()) {
                                        icon::paint(ui, tex);
                                    }
                                }
                                c.render(ui, &row, &pal);
                            });
                        }
                        let resp = table_row.response();
                        if resp.clicked() {
                            self.selected = Some(row.pid);
                        }
                        if resp.double_clicked() && !row.exe.is_empty() {
                            open_location(&row.exe);
                        }
                        resp.context_menu(|ui| {
                            if ui.button("タスクの終了").clicked() {
                                self.ask_kill(row.pid);
                                ui.close();
                            }
                            if ui.button("プロセス ツリーの終了").clicked() {
                                self.confirm_kill_tree = Some((row.pid, row.name.clone()));
                                ui.close();
                            }
                            if ui
                                .add_enabled(!row.exe.is_empty(), egui::Button::new("ファイルの場所を開く"))
                                .clicked()
                            {
                                open_location(&row.exe);
                                ui.close();
                            }
                            if ui
                                .add_enabled(!row.cmd.is_empty(), egui::Button::new("コマンドラインをコピー"))
                                .clicked()
                            {
                                ui.ctx().copy_text(row.cmd.clone());
                                ui.close();
                            }
                        });
                    });
                }
            });
    }

    /// パフォーマンスタブの左ナビ 1 項目 (ミニグラフ付き:
    /// 左にミニグラフ、右に名前・補足・値の 3 行)。
    fn perf_nav_item(
        ui: &mut egui::Ui,
        label: &str,
        detail: &str,
        value: &str,
        hist: &VecDeque<f32>,
        color: Color32,
        selected: bool,
        style: util::GraphStyle,
        pal: &Palette,
    ) -> egui::Response {
        let width = ui.available_width().clamp(80.0, 300.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 84.0), egui::Sense::click());
        Self::paint_nav_background(ui, rect, resp.hovered(), selected, color, 84.0, pal);
        let graph_rect = Self::nav_graph_rect(rect);
        util::paint_mini_graph(ui.painter(), graph_rect, hist, 100.0, color, style);
        Self::paint_nav_text(ui, rect, label, detail, value, color);
        resp
    }

    /// ディスク用ナビ項目 (読み取り/書き込みの 2 本線ミニグラフ)。
    #[allow(clippy::too_many_arguments)]
    fn perf_nav_item_dual(
        ui: &mut egui::Ui,
        label: &str,
        detail: &str,
        value: &str,
        read: &VecDeque<f32>,
        write: &VecDeque<f32>,
        max: f32,
        color: Color32,
        read_color: Color32,
        write_color: Color32,
        selected: bool,
        style: util::GraphStyle,
        pal: &Palette,
    ) -> egui::Response {
        let width = ui.available_width().clamp(80.0, 300.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 84.0), egui::Sense::click());
        Self::paint_nav_background(ui, rect, resp.hovered(), selected, color, 84.0, pal);
        let graph_rect = Self::nav_graph_rect(rect);
        util::paint_mini_dual_graph(ui.painter(), graph_rect, read, write, max, read_color, write_color, style);
        Self::paint_nav_text(ui, rect, label, detail, value, color);
        resp
    }

    fn nav_graph_rect(rect: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.top() + 10.0),
            egui::pos2(rect.left() + 96.0, rect.bottom() - 10.0),
        )
    }

    fn paint_nav_background(
        ui: &mut egui::Ui,
        rect: egui::Rect,
        hovered: bool,
        selected: bool,
        color: Color32,
        height: f32,
        pal: &Palette,
    ) {
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
            // 選択枠
            painter.rect_stroke(
                rect.shrink(1.0),
                3.0,
                egui::Stroke::new(1.0, color),
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(rect.left(), rect.top() + 10.0),
                    egui::pos2(rect.left() + 3.5, rect.top() + height - 10.0),
                ),
                1.5,
                color,
            );
        } else if hovered {
            painter.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
        }
        // グラフ枠
        let g = Self::nav_graph_rect(rect);
        painter.rect_filled(g, 2.0, ui.visuals().extreme_bg_color);
        painter.rect_stroke(
            g,
            2.0,
            egui::Stroke::new(1.0, pal.primary.gamma_multiply(0.35)),
            egui::StrokeKind::Inside,
        );
    }

    fn paint_nav_text(
        ui: &mut egui::Ui,
        rect: egui::Rect,
        label: &str,
        detail: &str,
        value: &str,
        color: Color32,
    ) {
        let painter = ui.painter();
        let x = rect.left() + 106.0;
        let wrap_width = (rect.right() - x - 8.0).max(40.0);
        painter.text(
            egui::pos2(x, rect.top() + 8.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );
        let mut value_y = rect.top() + 50.0;
        if !detail.is_empty() {
            // 長い GPU / NPU 名は最大 2 行で折り返し、それ以上は「…」で省略
            let mut job = egui::text::LayoutJob::simple(
                detail.to_owned(),
                egui::FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
                wrap_width,
            );
            job.wrap = egui::text::TextWrapping {
                max_width: wrap_width,
                max_rows: 2,
                overflow_character: Some('…'),
                ..Default::default()
            };
            let galley = painter.layout_job(job);
            let detail_y = rect.top() + 27.0;
            painter.galley(
                egui::pos2(x, detail_y),
                galley.clone(),
                ui.visuals().weak_text_color(),
            );
            value_y = (detail_y + galley.size().y + 3.0).max(value_y);
        }
        if !value.is_empty() {
            painter.text(
                egui::pos2(x, value_y.min(rect.bottom() - 15.0)),
                egui::Align2::LEFT_TOP,
                value,
                egui::FontId::proportional(13.0),
                color,
            );
        }
    }

    /// 詳細情報の 1 行 (ラベル左・値は固定幅の右側)。
    fn detail_row(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
        ui.horizontal(|ui| {
            let resp = ui.label(
                RichText::new(label)
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space((170.0 - resp.rect.width()).max(10.0));
            ui.label(RichText::new(value).size(13.0).monospace());
        });
    }

    /// いずれかのモーダルダイアログが開いているか。
    fn any_modal(&self) -> bool {
        self.settings_open
            || self.smart_dialog_disk.is_some()
            || self.confirm_kill.is_some()
            || self.confirm_kill_tree.is_some()
            || self.confirm_delete.is_some()
    }

    /// モーダルダイアログ表示中用の背景オーバーレイ。
    /// 画面全体をグレーに塗り、背面のウィジェットへのクリックを遮断する。
    fn modal_backdrop(ctx: &egui::Context) {
        let screen = ctx.content_rect();
        egui::Area::new(egui::Id::unique("minitask_modal_backdrop"))
            .order(egui::Order::Foreground)
            // 非インタラクティブにしないとクリックで move_to_top され、
            // backdrop がダイアログより上に来て操作不能になる
            .interactable(false)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let _ = ui.allocate_rect(screen, egui::Sense::hover());
                ui.painter().rect_filled(
                    screen,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_black_alpha(100),
                );
            });
    }

    /// 色付きの値を持つ詳細行 (SMART の健康状態・温度など)。
    fn detail_row_colored(
        ui: &mut egui::Ui,
        label: &str,
        value: impl Into<String>,
        color: egui::Color32,
    ) {
        ui.horizontal(|ui| {
            let resp = ui.label(
                RichText::new(label)
                    .size(12.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space((170.0 - resp.rect.width()).max(10.0));
            ui.label(RichText::new(value.into()).size(13.0).monospace().color(color));
        });
    }

    /// CPU グラフの右クリックメニュー (グラフ種別の切替)。
    fn cpu_graph_menu(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("グラフの変更").strong());
        ui.separator();
        let modes = [
            (CpuGraphMode::Overall, "全体的な使用率"),
            (CpuGraphMode::PerCore, "論理プロセッサ"),
        ];
        for (mode, label) in modes {
            let mark = if self.cpu_graph_mode == mode { "✓ " } else { "   " };
            if ui.button(format!("{mark}{label}")).clicked() {
                self.cpu_graph_mode = mode;
                ui.close();
            }
        }
        Self::update_speed_menu(
            ui,
            &mut self.interval,
            &mut self.paused,
            &mut self.graph_animation,
            &mut self.graph_spline,
        );
    }

    /// 「更新の速度」メニュー (高/通常/低/一時停止)。
    fn update_speed_menu(
        ui: &mut egui::Ui,
        interval: &mut f32,
        paused: &mut bool,
        animation: &mut bool,
        spline: &mut bool,
    ) {
        ui.separator();
        ui.label(RichText::new("更新の速度").strong());
        for (label, value) in [
            ("0.5 秒", 0.5_f32),
            ("1 秒 (通常)", 1.0),
            ("5 秒 (低)", 5.0),
        ] {
            let mark = if !*paused && (*interval - value).abs() < 0.01 {
                "✓ "
            } else {
                "   "
            };
            if ui.button(format!("{mark}{label}")).clicked() {
                *interval = value;
                *paused = false;
                ui.close();
            }
        }
        let mark = if *paused { "✓ " } else { "   " };
        if ui.button(format!("{mark}一時停止と再開")).clicked() {
            *paused = !*paused;
            ui.close();
        }
        ui.separator();
        ui.checkbox(animation, "なめらかなアニメーション補間");
        ui.checkbox(spline, "スプライン補間 (滑らかな曲線)");
    }

    /// GPU / NPU グラフの右クリックメニュー (種別グラフへの切替:
    /// 全体的な使用率 + エンジン種別ごとのグラフ)。
    fn engine_graph_menu(
        ui: &mut egui::Ui,
        sel: &mut Option<String>,
        engines: &[(String, f32)],
        npu: bool,
        interval: &mut f32,
        paused: &mut bool,
        animation: &mut bool,
        spline: &mut bool,
    ) {
        ui.label(RichText::new("グラフの変更").strong());
        ui.separator();
        let mark = if sel.is_none() { "✓ " } else { "   " };
        if ui.button(format!("{mark}全体的な使用率")).clicked() {
            *sel = None;
            ui.close();
        }
        for (key, _) in engines {
            let name = if npu {
                crate::perf::npu_engine_display_name(key)
            } else {
                crate::perf::engine_display_name(key)
            };
            let mark = if sel.as_deref() == Some(key.as_str()) { "✓ " } else { "   " };
            if ui.button(format!("{mark}{name}")).clicked() {
                *sel = Some(key.clone());
                ui.close();
            }
        }
        Self::update_speed_menu(ui, interval, paused, animation, spline);
    }

    fn performance_tab(&mut self, ui: &mut egui::Ui) {
        let mem_pct = if self.load.mem_total > 0 {
            self.load.mem_used as f32 / self.load.mem_total as f32 * 100.0
        } else {
            0.0
        };
        // なめらか補間の位相 (0..1): 高速サンプリング (250ms) の進み具合
        let phase = if self.graph_animation && !self.paused {
            (self.last_hist_tick.elapsed().as_secs_f32() / 0.25).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let style = util::GraphStyle {
            phase,
            spline: self.graph_spline,
        };

        // --- 左ナビ: 接続中のデバイスのみ表示 (切断時は自動的に消える) ---
        let empty_hist = VecDeque::new();
        // ディスクは読み取り/書き込み 2 本線グラフを使う
        let disk_dual = match &self.perf_sel {
            PerfSel::Disk(id) => {
                let ah = find_hist(&self.disk_active_hist, id, &empty_hist).clone();
                let rh = find_hist(&self.disk_read_hist, id, &empty_hist).clone();
                let wh = find_hist(&self.disk_write_hist, id, &empty_hist).clone();
                let max = util::nice_max(&rh, &wh);
                Some((ah, rh, wh, max))
            }
            _ => None,
        };
        // ネットワークは受信/送信 2 本線グラフを使う
        let net_dual = match &self.perf_sel {
            PerfSel::Net(id) => {
                let rh = find_hist(&self.net_recv_hist, id, &empty_hist).clone();
                let sh = find_hist(&self.net_sent_hist, id, &empty_hist).clone();
                let max = util::nice_max(&rh, &sh);
                Some((rh, sh, max))
            }
            _ => None,
        };
        // (選択項目, 名前, 補足, 値, 履歴, 色)
        type NavItem<'a> = (PerfSel, String, String, String, &'a VecDeque<f32>, Color32);
        let top_items: Vec<NavItem<'_>> = vec![
            (
                PerfSel::Cpu,
                "CPU".to_owned(),
                format!("{:.2} GHz", self.load.cpu_freq_mhz as f64 / 1000.0),
                format!("{:.0}%", self.load.cpu),
                &self.cpu_hist,
                self.palette.cpu,
            ),
            (
                PerfSel::Ram,
                "メモリ".to_owned(),
                format!(
                    "{}/{} ({}%)",
                    util::fmt_bytes(self.load.mem_used),
                    util::fmt_bytes(self.load.mem_total),
                    mem_pct as u32
                ),
                String::new(),
                &self.mem_hist,
                self.palette.mem,
            ),
        ];
        let mut bottom_items: Vec<NavItem<'_>> = Vec::new();
        for g in &self.load.gpus {
            let h = find_hist(&self.gpu_hist, &g.id, &empty_hist);
            bottom_items.push((
                PerfSel::Gpu(g.id.clone()),
                g.label.clone(),
                g.model.clone(),
                format!("{:.0}%", g.usage),
                h,
                self.palette.gpu,
            ));
        }
        for n in &self.load.npus {
            let h = find_hist(&self.npu_hist, &n.id, &empty_hist);
            bottom_items.push((
                PerfSel::Npu(n.id.clone()),
                n.label.clone(),
                n.name.clone(),
                format!("{:.0}%", n.usage),
                h,
                self.palette.npu,
            ));
        }
        let mut clicked_sel: Option<PerfSel> = None;
        egui::Panel::left("perf_nav")
            .exact_size(280.0)
            .resizable(false)
            .drag_to_open(false)
            .show(ui, |ui| {
                // デバイスが増えても左ナビからアクセスできるようスクロール可能に
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                ui.add_space(4.0);
                for (sel, label, detail, value, hist, color) in top_items {
                    let resp = Self::perf_nav_item(
                        ui,
                        &label,
                        &detail,
                        &value,
                        hist,
                        color,
                        self.perf_sel == sel,
                        style,
                        &self.palette,
                    );
                    if resp.clicked() {
                        clicked_sel = Some(sel);
                    }
                }
                // ディスク: 物理ディスク単位、読み取り/書き込みの 2 本線グラフ
                for d in &self.load.disks {
                    let rh = find_hist(&self.disk_read_hist, &d.id, &empty_hist);
                    let wh = find_hist(&self.disk_write_hist, &d.id, &empty_hist);
                    let max = util::nice_max(rh, wh);
                    let val = util::fmt_rate(d.read_bps.max(d.write_bps));
                    let sel = PerfSel::Disk(d.id.clone());
                    let resp = Self::perf_nav_item_dual(
                        ui,
                        &d.label,
                        &d.type_label,
                        &val,
                        rh,
                        wh,
                        max,
                        self.palette.disk_write,
                        self.palette.cpu,
                        self.palette.disk_write,
                        self.perf_sel == sel,
                        style,
                        &self.palette,
                    );
                    if resp.clicked() {
                        clicked_sel = Some(sel);
                    }
                }
                // ネットワーク: アダプターごと、受信(緑)/送信(黄) の 2 本線
                for a in &self.load.adapters {
                    let rh = find_hist(&self.net_recv_hist, &a.id, &empty_hist);
                    let sh = find_hist(&self.net_sent_hist, &a.id, &empty_hist);
                    let max = util::nice_max(rh, sh);
                    let val = util::fmt_rate(a.recv_bps.max(a.sent_bps));
                    let sel = PerfSel::Net(a.id.clone());
                    let detail = if a.is_wifi { "Wi-Fi" } else { "イーサネット" };
                    let resp = Self::perf_nav_item_dual(
                        ui,
                        &a.label,
                        detail,
                        &val,
                        rh,
                        sh,
                        max,
                        self.palette.net_recv,
                        self.palette.net_recv,
                        self.palette.net_send,
                        self.perf_sel == sel,
                        style,
                        &self.palette,
                    );
                    if resp.clicked() {
                        clicked_sel = Some(sel);
                    }
                }
                for (sel, label, detail, value, hist, color) in bottom_items {
                    let resp = Self::perf_nav_item(
                        ui,
                        &label,
                        &detail,
                        &value,
                        hist,
                        color,
                        self.perf_sel == sel,
                        style,
                        &self.palette,
                    );
                    if resp.clicked() {
                        clicked_sel = Some(sel);
                    }
                }
                    });
            });
        if let Some(sel) = clicked_sel {
            self.perf_sel = sel;
            self.gpu_sel = None;
            self.npu_sel = None;
        }

        // ScrollArea の内側では available_height() が実質無限大になるため、
        // 入る前に実ビューポート高さを記録する。詳細行・余白の実測高さは
        // 毎フレーム自動補正し、収まるときはスクロールバーを出さない。
        let viewport_h = ui.available_height().clamp(0.0, 4000.0);
        let overhead = self.perf_overhead;
        let mut content_h = 0.0_f32;
        // 右側の内容は四周に余白を取り、縦にはみ出したらスクロールできるようにする
        egui::Frame::default()
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 12,
            })
            .show(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
        // --- 中央: 選択中メトリクスの大きなグラフと詳細 ---
        // GPU/NPU はエンジン種別が選択されていればそのグラフを表示する
        let (title, subtitle, value, hist, color) = match &self.perf_sel {
            PerfSel::Cpu => (
                "CPU".to_owned(),
                if self.load.cpu_brand.is_empty() {
                    "プロセッサ".to_owned()
                } else {
                    self.load.cpu_brand.clone()
                },
                format!("{:.0}%", self.load.cpu),
                &self.cpu_hist,
                self.palette.cpu,
            ),
            PerfSel::Ram => (
                "メモリ".to_owned(),
                "物理メモリ (RAM)".to_owned(),
                format!("{mem_pct:.0}%"),
                &self.mem_hist,
                self.palette.mem,
            ),
            PerfSel::Disk(id) => {
                let d = self.load.disks.iter().find(|d| &d.id == id);
                let title = d
                    .map(|d| d.label.clone())
                    .unwrap_or_else(|| "ディスク".to_owned());
                let value = d
                    .map(|d| format!("{:.0}%", d.active_pct))
                    .unwrap_or_else(|| "0%".to_owned());
                let subtitle = d
                    .map(|d| d.type_label.clone())
                    .unwrap_or_else(|| "ストレージ".to_owned());
                let h = find_hist(&self.disk_active_hist, id, &empty_hist);
                (title, subtitle, value, h, self.palette.mem)
            }
            PerfSel::Gpu(id) => {
                let g = self.load.gpus.iter().find(|g| &g.id == id);
                let model = g
                    .map(|g| g.model.clone())
                    .unwrap_or_else(|| "GPU".to_owned());
                let label = g
                    .map(|g| g.label.clone())
                    .unwrap_or_else(|| "GPU".to_owned());
                let usage = g.map(|g| g.usage).unwrap_or(0.0);
                let overall = (
                    label.clone(),
                    model.clone(),
                    format!("{usage:.0}%"),
                    find_hist(&self.gpu_hist, id, &empty_hist),
                    self.palette.gpu,
                );
                if let (Some(g), Some(key)) = (g, self.gpu_sel.clone()) {
                    if let Some((_, v)) = g.engines.iter().find(|(k, _)| *k == key) {
                        let name = crate::perf::engine_display_name(&key);
                        let h = find_hist(&self.gpu_engine_hist, &key, &empty_hist);
                        (
                            format!("{label} — {name}"),
                            model,
                            format!("{v:.0}%"),
                            h,
                            self.palette.gpu,
                        )
                    } else {
                        overall
                    }
                } else {
                    overall
                }
            }
            PerfSel::Npu(id) => {
                let n = self.load.npus.iter().find(|n| &n.id == id);
                let label = n
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| "NPU".to_owned());
                let name = n.map(|n| n.name.clone()).unwrap_or_default();
                let usage = n.map(|n| n.usage).unwrap_or(0.0);
                let npu_sub = if self.load.npu_available {
                    "NPU エンジン".to_owned()
                } else {
                    name
                };
                let overall = (
                    label.clone(),
                    npu_sub.clone(),
                    format!("{usage:.0}%"),
                    find_hist(&self.npu_hist, id, &empty_hist),
                    self.palette.npu,
                );
                if let Some(key) = self.npu_sel.clone() {
                    let cur = n.and_then(|n| n.engines.iter().find(|(k, _)| *k == key));
                    if let Some((_, v)) = cur {
                        let dname = crate::perf::npu_engine_display_name(&key);
                        let hist_key = format!("{id}|{key}");
                        let h = find_hist(&self.npu_engine_hist, &hist_key, &empty_hist);
                        (
                            format!("{label} — {dname}"),
                            npu_sub,
                            format!("{v:.0}%"),
                            h,
                            self.palette.npu,
                        )
                    } else {
                        overall
                    }
                } else {
                    overall
                }
            }
            PerfSel::Net(id) => {
                let a = self.load.adapters.iter().find(|x| &x.id == id);
                let title = a
                    .map(|a| a.label.clone())
                    .unwrap_or_else(|| "ネットワーク".to_owned());
                let subtitle = a
                    .map(|a| a.description.clone())
                    .unwrap_or_else(|| "ネットワーク アダプター".to_owned());
                let value = a
                    .map(|a| util::fmt_rate(a.recv_bps.max(a.sent_bps)))
                    .unwrap_or_else(|| "0 KB/秒".to_owned());
                let h = find_hist(&self.net_recv_hist, id, &empty_hist);
                (title, subtitle, value, h, self.palette.net_recv)
            }
        };

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(title).size(22.0).strong().color(color));
                ui.label(
                    RichText::new(subtitle)
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(value)
                        .size(32.0)
                        .strong()
                        .monospace()
                        .color(color),
                );
            });
        });
        ui.add_space(10.0);
        // ヘッダー・ラベル・詳細情報 (最大 6 行)・区切り線・余白の予約分 ~320px
        let avail_h = viewport_h;
        let gh = (avail_h * 0.52).min(avail_h - overhead).clamp(80.0, 340.0);
        let gw = ui.available_width().clamp(60.0, 4000.0);

        if self.perf_sel == PerfSel::Cpu && self.cpu_graph_mode == CpuGraphMode::PerCore {
            // 論理プロセッサごとのグラフ
            let n = self.core_hist.len().max(1);
            let rows = if n <= 4 { 1 } else { 2 };
            let cols = n.div_ceil(rows);
            // 行間には egui が item_spacing.y を挿入するので考慮する
            let row_gap = ui.spacing().item_spacing.y;
            let ch = ((gh - (rows as f32 - 1.0) * row_gap) / rows as f32).max(36.0);
            const CELL_GAP: f32 = 6.0;
            for row in 0..rows {
                // 1 行分の領域をまとめて確保し、セルは内側に手動配置する。
                // (セルごとに allocate_exact_size すると item_spacing.x が
                //  自動挿入され、右端がはみ出すため)
                let w = ui.available_width().clamp(0.0, 8000.0);
                let (row_rect, row_resp) =
                    ui.allocate_exact_size(Vec2::new(w, ch), egui::Sense::click());
                let cw = ((w - (cols as f32 - 1.0) * CELL_GAP) / cols as f32).max(24.0);
                for col in 0..cols {
                    let idx = row * cols + col;
                    if idx >= self.core_hist.len() {
                        break;
                    }
                    let left = row_rect.left() + col as f32 * (cw + CELL_GAP);
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(left, row_rect.top()),
                        Vec2::new(cw, ch),
                    );
                    if rect.right() > row_rect.right() + 1.0 {
                        break;
                    }
                    let painter = ui.painter();
                    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
                    let cur = self.load.core_usage.get(idx).copied().unwrap_or(0.0);
                    painter.text(
                        rect.left_top() + egui::vec2(8.0, 4.0),
                        egui::Align2::LEFT_TOP,
                        format!("CPU {idx}"),
                        egui::FontId::proportional(11.0),
                        ui.visuals().weak_text_color(),
                    );
                    painter.text(
                        egui::pos2(rect.right() - 8.0, rect.top() + 4.0),
                        egui::Align2::RIGHT_TOP,
                        format!("{cur:.0}%"),
                        egui::FontId::proportional(11.0),
                        self.palette.cpu,
                    );
                    let graph_rect = egui::Rect::from_min_max(
                        egui::pos2(rect.left() + 6.0, rect.top() + 20.0),
                        egui::pos2(rect.right() - 6.0, rect.bottom() - 4.0),
                    );
                    util::paint_mini_graph(
                        painter,
                        graph_rect,
                        &self.core_hist[idx],
                        100.0,
                        self.palette.cpu,
                        style,
                    );
                }
                // 論理プロセッサ表示でも右クリックで「全体的な使用率」に戻せる
                row_resp.context_menu(|ui| self.cpu_graph_menu(ui));
            }
        } else if let Some((ah, rh, wh, max)) = disk_dual {
            // 2 段構成:
            // 上 = アクティブな時間 (%)、下 = ディスクの転送速度 (読み取り/書き込み)
            let main_h = (gh * 0.62).max(80.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("アクティブな時間")
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
            util::perf_chart(ui, &ah, 100.0, self.palette.mem, Vec2::new(gw, main_h), style)
                .context_menu(|ui| {
                    Self::update_speed_menu(
                        ui,
                        &mut self.interval,
                        &mut self.paused,
                        &mut self.graph_animation,
                        &mut self.graph_spline,
                    )
                });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("ディスクの転送速度")
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{}/秒", util::fmt_bytes(max as u64)))
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });
            util::perf_dual_chart(
                ui,
                &rh,
                &wh,
                max,
                self.palette.cpu,
                self.palette.disk_write,
                Vec2::new(gw, (gh - main_h - 12.0).max(60.0)),
                style,
            );
        } else if let Some((rh, sh, max)) = net_dual {
            // ネットワーク: 送信(黄) / 受信(緑) の 2 本線 (凡例付き)
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("送信 / 受信")
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{}/秒", util::fmt_bytes(max as u64)))
                            .size(11.0)
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });
            util::perf_dual_chart(
                ui,
                &rh,
                &sh,
                max,
                self.palette.net_recv,
                self.palette.net_send,
                Vec2::new(gw, gh),
                style,
            )
            .context_menu(|ui| {
                Self::update_speed_menu(
                    ui,
                    &mut self.interval,
                    &mut self.paused,
                    &mut self.graph_animation,
                    &mut self.graph_spline,
                )
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("■ 送信").size(11.0).color(self.palette.net_send));
                ui.label(RichText::new("■ 受信").size(11.0).color(self.palette.net_recv));
            });
        } else {
            let resp = util::perf_chart(ui, hist, 100.0, color, Vec2::new(gw, gh), style);
            match &self.perf_sel {
                PerfSel::Cpu => {
                    resp.on_hover_text("右クリックでグラフの変更")
                        .context_menu(|ui| self.cpu_graph_menu(ui));
                }
                PerfSel::Gpu(id) => {
                    let engines = self
                        .load
                        .gpus
                        .iter()
                        .find(|g| &g.id == id)
                        .map(|g| g.engines.clone())
                        .unwrap_or_default();
                    let sel = &mut self.gpu_sel;
                    let interval = &mut self.interval;
                    let paused = &mut self.paused;
                    let animation = &mut self.graph_animation;
                    let spline = &mut self.graph_spline;
                    resp.on_hover_text("右クリックでグラフの変更 (エンジン種別)")
                        .context_menu(move |ui| {
                            Self::engine_graph_menu(
                                ui,
                                sel,
                                &engines,
                                false,
                                interval,
                                paused,
                                animation,
                                spline,
                            )
                        });
                    // 「専用 GPU メモリ」の第2グラフ
                    let mem_hist = self.gpu_mem_hist.clone();
                    let mem_max = util::nice_max(&mem_hist, &mem_hist);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("専用 GPU メモリ")
                                .size(12.0)
                                .color(ui.visuals().weak_text_color()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(util::fmt_bytes(mem_max as u64))
                                    .size(11.0)
                                    .color(ui.visuals().weak_text_color()),
                            );
                        });
                    });
                    util::perf_dual_chart(
                        ui,
                        &mem_hist,
                        &mem_hist,
                        mem_max,
                        self.palette.gpu,
                        self.palette.gpu,
                        Vec2::new(gw, (gh * 0.3).max(50.0)),
                        style,
                    );
                }
                PerfSel::Npu(id) => {
                    let engines = self
                        .load
                        .npus
                        .iter()
                        .find(|n| &n.id == id)
                        .map(|n| n.engines.clone())
                        .unwrap_or_default();
                    if !engines.is_empty() {
                        let sel = &mut self.npu_sel;
                        let interval = &mut self.interval;
                        let paused = &mut self.paused;
                        let animation = &mut self.graph_animation;
                        let spline = &mut self.graph_spline;
                        resp.on_hover_text("右クリックでグラフの変更 (エンジン種別)")
                            .context_menu(move |ui| {
                                Self::engine_graph_menu(
                                    ui,
                                    sel,
                                    &engines,
                                    true,
                                    interval,
                                    paused,
                                    animation,
                                    spline,
                                )
                            });
                    }
                    // NPU 専用メモリの第2グラフ
                    let mem_hist = self.npu_mem_hist.clone();
                    let mem_max = util::nice_max(&mem_hist, &mem_hist);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("専用 NPU メモリ")
                                .size(12.0)
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
                    util::perf_dual_chart(
                        ui,
                        &mem_hist,
                        &mem_hist,
                        mem_max,
                        self.palette.npu,
                        self.palette.npu,
                        Vec2::new(gw, (gh * 0.3).max(50.0)),
                        style,
                    );
                }
                _ => {
                    resp.context_menu(|ui| {
                        Self::update_speed_menu(
                            ui,
                            &mut self.interval,
                            &mut self.paused,
                            &mut self.graph_animation,
                            &mut self.graph_spline,
                        )
                    });
                }
            }
        }
        ui.add_space(10.0);
        ui.separator();

        // 詳細情報 (2 列)
        let d = &self.load;
        let gb = util::fmt_bytes;
        let pal = self.palette;
        let mut smart_open: Option<String> = None;
        ui.columns(2, |cols| {
            let (l, rest) = cols.split_first_mut().unwrap();
            let r = rest.first_mut().unwrap();
            l.vertical(|ui| match &self.perf_sel {
                PerfSel::Cpu => {
                    Self::detail_row(ui, "利用率", format!("{:.1}%", d.cpu));
                    Self::detail_row(
                        ui,
                        "速度",
                        format!("{:.2} GHz", d.cpu_freq_mhz as f64 / 1000.0),
                    );
                    Self::detail_row(ui, "プロセス数", d.processes.to_string());
                    Self::detail_row(ui, "ハンドル数", d.handles.to_string());
                }
                PerfSel::Ram => {
                    Self::detail_row(ui, "使用中", gb(d.mem_used));
                    Self::detail_row(ui, "利用可能", gb(d.mem_available));
                    Self::detail_row(
                        ui,
                        "コミット済み",
                        format!("{} / {}", gb(d.commit_used), gb(d.commit_limit)),
                    );
                    Self::detail_row(ui, "キャッシュ済み", gb(d.cached));
                    Self::detail_row(ui, "ページ プール", gb(d.kernel_paged));
                    Self::detail_row(ui, "非ページ プール", gb(d.kernel_nonpaged));
                }
                PerfSel::Disk(id) => {
                    if let Some(disk) = d.disks.iter().find(|x| &x.id == id) {
                        Self::detail_row(
                            ui,
                            "アクティブな時間",
                            format!("{:.0}%", disk.active_pct),
                        );
                        Self::detail_row(
                            ui,
                            "平均応答時間",
                            format!("{:.1} ミリ秒", disk.response_ms),
                        );
                        Self::detail_row(ui, "読み取り速度", util::fmt_rate(disk.read_bps));
                        Self::detail_row(ui, "書き込み速度", util::fmt_rate(disk.write_bps));
                        if let Some(smart) = &disk.smart {
                            use crate::smart::SmartStatus;
                            let health_color = match smart.status {
                                SmartStatus::Good => pal.mem,
                                SmartStatus::Caution => pal.warn,
                                SmartStatus::Bad => pal.danger,
                                SmartStatus::Unknown => ui.visuals().weak_text_color(),
                            };
                            Self::detail_row_colored(
                                ui,
                                "健康状態",
                                smart.status_text(),
                                health_color,
                            );
                            if let Some(temp) = smart.temperature_c {
                                let temp_color = if temp >= 70 {
                                    pal.danger
                                } else if temp >= 60 {
                                    pal.warn
                                } else {
                                    pal.cpu
                                };
                                Self::detail_row_colored(
                                    ui,
                                    "温度",
                                    format!("{temp} °C"),
                                    temp_color,
                                );
                            }
                            if ui.button("SMART 詳細...").clicked() {
                                smart_open = Some(id.clone());
                            }
                        }
                    }
                }
                PerfSel::Gpu(id) => {
                    if let Some(g) = d.gpus.iter().find(|x| &x.id == id) {
                        Self::detail_row(ui, "使用率", format!("{:.1}%", g.usage));
                        Self::detail_row(
                            ui,
                            "専用 GPU メモリ",
                            format!("{} / {}", gb(g.mem_used), gb(g.mem_total)),
                        );
                        Self::detail_row(
                            ui,
                            "共有 GPU メモリ",
                            format!("{} / {}", gb(d.gpu_shared), gb(d.mem_total)),
                        );
                        Self::detail_row(ui, "専用メモリ使用", gb(d.gpu_dedicated));
                    }
                }
                PerfSel::Npu(id) => {
                    let n = d.npus.iter().find(|x| &x.id == id);
                    Self::detail_row(
                        ui,
                        "使用率",
                        format!("{:.1}%", n.map(|n| n.usage).unwrap_or(0.0)),
                    );
                    Self::detail_row(ui, "専用 NPU メモリ", gb(d.npu_dedicated));
                    Self::detail_row(ui, "共有 NPU メモリ", gb(d.npu_shared));
                    Self::detail_row(
                        ui,
                        "状態",
                        if d.npu_available {
                            "計測中"
                        } else if d.npu_device_present {
                            "搭載済み・使用率取得不可"
                        } else {
                            "非搭載"
                        },
                    );
                }
                PerfSel::Net(id) => {
                    if let Some(a) = d.adapters.iter().find(|x| &x.id == id) {
                        Self::detail_row(ui, "送信", util::fmt_rate(a.sent_bps));
                        Self::detail_row(ui, "受信", util::fmt_rate(a.recv_bps));
                        if a.is_wifi {
                            Self::detail_row(
                                ui,
                                "信号の品質",
                                format!("{}%", a.signal_pct),
                            );
                        }
                    }
                }
            });
            r.vertical(|ui| match &self.perf_sel {
                PerfSel::Cpu => {
                    Self::detail_row(ui, "スレッド数", d.threads.to_string());
                    let s = d.uptime_secs;
                    Self::detail_row(
                        ui,
                        "稼働時間",
                        format!("{}:{:02}:{:02}:{:02}", s / 86400, s / 3600 % 24, s / 60 % 60, s % 60),
                    );
                    Self::detail_row(ui, "論理プロセッサ数", d.logical_cores.to_string());
                    Self::detail_row(ui, "更新間隔", format!("{} 秒", self.interval));
                }
                PerfSel::Ram => {
                    Self::detail_row(ui, "合計 (物理)", gb(d.mem_total));
                    Self::detail_row(ui, "プロセス数", d.processes.to_string());
                }
                PerfSel::Disk(id) => {
                    if let Some(disk) = d.disks.iter().find(|x| &x.id == id) {
                        Self::detail_row(ui, "容量", gb(disk.total));
                        Self::detail_row(ui, "フォーマット済み", gb(disk.total));
                        let system = disk.letters.iter().any(|l| l == "C");
                        Self::detail_row(
                            ui,
                            "システム ディスク",
                            if system { "はい" } else { "いいえ" },
                        );
                        Self::detail_row(
                            ui,
                            "ページ ファイル",
                            if system { "はい" } else { "いいえ" },
                        );
                        Self::detail_row(ui, "種類", disk.type_label.clone());
                        Self::detail_row(ui, "空き容量", gb(disk.free));
                    }
                }
                PerfSel::Gpu(id) => {
                    if let Some(g) = d.gpus.iter().find(|x| &x.id == id) {
                        Self::detail_row(ui, "モデル", g.model.clone());
                        let top = g.engines.first();
                        Self::detail_row(
                            ui,
                            "最も使用された GPU エンジン",
                            match top {
                                Some((k, v)) => {
                                    format!("{} ({v:.1}%)", crate::perf::engine_display_name(k))
                                }
                                None => "—".to_owned(),
                            },
                        );
                    }
                }
                PerfSel::Npu(id) => {
                    let has_counter = d
                        .npus
                        .iter()
                        .find(|x| &x.id == id)
                        .map(|n| !n.engines.is_empty())
                        .unwrap_or(false);
                    Self::detail_row(
                        ui,
                        "カウンタ",
                        if has_counter {
                            "NPU Engine"
                        } else if d.npu_device_present {
                            "未提供 (デバイスは搭載済み)"
                        } else {
                            "—"
                        },
                    );
                }
                PerfSel::Net(id) => {
                    if let Some(a) = d.adapters.iter().find(|x| &x.id == id) {
                        Self::detail_row(ui, "アダプター名", a.label.clone());
                        Self::detail_row(
                            ui,
                            "IPv4 アドレス",
                            if a.ipv4.is_empty() {
                                "—".to_owned()
                            } else {
                                a.ipv4.join(", ")
                            },
                        );
                        Self::detail_row(
                            ui,
                            "IPv6 アドレス",
                            if a.ipv6.is_empty() {
                                "—".to_owned()
                            } else {
                                a.ipv6.join(", ")
                            },
                        );
                        let speed = a.link_speed_bps as f64 / 1_000_000.0;
                        Self::detail_row(
                            ui,
                            "帯域幅",
                            if speed >= 1000.0 {
                                format!("{:.1} Gbps", speed / 1000.0)
                            } else {
                                format!("{speed:.1} Mbps")
                            },
                        );
                        Self::detail_row(ui, "状態", a.status.clone());
                        if a.is_wifi {
                            Self::detail_row(
                                ui,
                                "SSID",
                                if a.ssid.is_empty() {
                                    "—".to_owned()
                                } else {
                                    a.ssid.clone()
                                },
                            );
                        }
                        Self::detail_row(ui, "説明", a.description.clone());
                    }
                }
            });
        });
        if let Some(id) = smart_open {
            // 開いた瞬間の最新 SMART を取得してから表示する
            self.refresh_smart(&id);
            self.smart_dialog_disk = Some(id);
        }
        content_h = ui.min_rect().height();
        });
        });
        // 実測に基づき予約高さを補正 (数フレームで収まりに収束する)
        if content_h > 0.0 {
            // フレーム上下の余白 24px を含めて判定する
            let diff = content_h + 24.0 - viewport_h;
            if diff > 0.5 {
                self.perf_overhead = (self.perf_overhead + diff.min(48.0))
                    .min((viewport_h - 80.0).max(80.0));
            } else if diff < -60.0 {
                self.perf_overhead = (self.perf_overhead - 8.0).max(260.0);
            }
        }
    }

    fn startup_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("ログイン時に自動起動するアプリ:");
            if ui.button("再読み込み").clicked() {
                self.startup_entries = startup::list();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new("右クリックで削除 / 状態ボタンで有効・無効切替")
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });
        ui.add_space(2.0);

        let entries = self.startup_entries.clone();
        self.icon_cache.reset_frame();
        let mut entry_icons: HashMap<String, Option<egui::TextureHandle>> = HashMap::new();
        for entry in entries.iter().take(60) {
            let path = icon::exe_path_from_command(&entry.command);
            let key = if path.is_empty() {
                entry.name.clone()
            } else {
                path.clone()
            };
            entry_icons.insert(entry.name.clone(), self.icon_cache.get(ui.ctx(), &key, &path));
        }
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(180.0).clip(true)) // 名前
            .column(Column::initial(80.0)) // 状態
            .column(Column::initial(170.0).clip(true)) // 種類
            .column(Column::auto().at_least(320.0).clip(true)) // コマンド
            .header(24.0, |mut header| {
                header.col(|ui| {
                    ui.strong("名前");
                });
                header.col(|ui| {
                    ui.strong("状態");
                });
                header.col(|ui| {
                    ui.strong("種類");
                });
                header.col(|ui| {
                    ui.strong("コマンド / パス");
                });
            })
            .body(|mut body| {
                for entry in entries {
                    body.row(26.0, |mut row| {
                        row.col(|ui| {
                            if let Some(tex) = entry_icons.get(&entry.name).and_then(|t| t.as_ref()) {
                                icon::paint(ui, tex);
                            }
                            ui.label(&entry.name);
                        });
                        row.col(|ui| {
                            let label = if entry.enabled { "有効" } else { "無効" };
                            let color = if entry.enabled { self.palette.mem } else { ui.visuals().weak_text_color() };
                            if ui.button(RichText::new(label).color(color)).clicked() {
                                match startup::set_enabled(&entry, !entry.enabled) {
                                    Ok(()) => {
                                        self.startup_entries = startup::list();
                                        self.notify(format!(
                                            "「{}」を{}にしました",
                                            entry.name,
                                            if entry.enabled { "無効" } else { "有効" }
                                        ));
                                    }
                                    Err(e) => self.notify(format!("切替失敗: {e}")),
                                }
                            }
                        });
                        row.col(|ui| {
                            ui.label(entry.kind.label());
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(&entry.command).size(12.0));
                        });
                        row.response().context_menu(|ui| {
                            if ui.button("削除").clicked() {
                                self.confirm_delete = Some(entry.clone());
                                ui.close();
                            }
                        });
                    });
                }
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("プロセス: {}", self.rows.len()))
                    .size(12.0),
            );
            ui.separator();
            match &self.net {
                Some(session) if session.is_alive() => {
                    ui.label(
                        RichText::new("ネットワーク計測: 有効 (ETW)")
                            .size(12.0)
                            .color(self.palette.mem),
                    );
                }
                Some(_) => {
                    ui.label(
                        RichText::new("ネットワーク計測: セッション停止")
                            .size(12.0)
                            .color(self.palette.warn),
                    );
                }
                None => {
                    if let Some(err) = &self.net_err {
                        ui.label(
                            RichText::new(format!("ネットワーク計測: 無効 — {err}"))
                                .size(12.0)
                                .color(self.palette.warn),
                        );
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("refresh_interval")
                    .selected_text(format!("{} 秒", self.interval))
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for v in [0.5_f32, 1.0, 2.0, 5.0] {
                            ui.selectable_value(&mut self.interval, v, format!("{v} 秒"));
                        }
                    });
                ui.label(RichText::new("更新間隔:").size(12.0));
                if let Some((msg, _)) = &self.toast {
                    ui.separator();
                    ui.label(RichText::new(msg).size(12.0).color(self.palette.primary));
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // すべてのダイアログをモーダル化: 背景をグレーアウトし、Esc で閉じる
        if self.any_modal() {
            Self::modal_backdrop(ctx);
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.settings_open = false;
                self.smart_dialog_disk = None;
                self.confirm_kill = None;
                self.confirm_kill_tree = None;
                self.confirm_delete = None;
            }
        }
        if let Some((pid, name)) = self.confirm_kill.clone() {
            egui::Window::new("タスクの終了")
                .collapsible(false)
                .resizable(false)
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(format!("「{name}」(PID {pid}) を強制終了しますか？"));
                    ui.label(
                        RichText::new("保存されていないデータは失われます。")
                            .small()
                            .color(self.palette.warn),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("終了する").color(self.palette.danger))
                            .clicked()
                        {
                            match process::kill(pid) {
                                Ok(()) => self.notify(format!("PID {pid} を終了しました")),
                                Err(e) => self.notify(e),
                            }
                            self.confirm_kill = None;
                            self.selected = None;
                            self.refresh();
                        }
                        if ui.button("キャンセル").clicked() {
                            self.confirm_kill = None;
                        }
                    });
                });
        }

        if let Some((pid, name)) = self.confirm_kill_tree.clone() {
            egui::Window::new("プロセス ツリーの終了")
                .collapsible(false)
                .resizable(false)
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(340.0);
                    ui.label(format!("「{name}」(PID {pid}) とその子孫プロセスすべてを強制終了しますか？"));
                    ui.label(
                        RichText::new("対象プロセスが起動したすべての子プロセスも終了します。")
                            .small()
                            .color(self.palette.warn),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("終了する").color(self.palette.danger))
                            .clicked()
                        {
                            let rows = self.rows.clone();
                            match process::kill_tree(pid, &rows) {
                                Ok(()) => self.notify(format!("PID {pid} のツリーを終了しました")),
                                Err(e) => self.notify(e),
                            }
                            self.confirm_kill_tree = None;
                            self.selected = None;
                            self.refresh();
                        }
                        if ui.button("キャンセル").clicked() {
                            self.confirm_kill_tree = None;
                        }
                    });
                });
        }

        if let Some(entry) = self.confirm_delete.clone() {
            egui::Window::new("スタートアップから削除")
                .collapsible(false)
                .resizable(false)
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(340.0);
                    ui.label(format!("「{}」をスタートアップから削除しますか？", entry.name));
                    ui.label(
                        RichText::new("レジストリ値またはファイルが削除されます。")
                            .small()
                            .color(self.palette.warn),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("削除する").color(self.palette.danger))
                            .clicked()
                        {
                            match startup::delete(&entry) {
                                Ok(()) => {
                                    self.notify(format!("「{}」を削除しました", entry.name));
                                    self.startup_entries = startup::list();
                                }
                                Err(e) => self.notify(format!("削除失敗: {e}")),
                            }
                            self.confirm_delete = None;
                        }
                        if ui.button("キャンセル").clicked() {
                            self.confirm_delete = None;
                        }
                    });
                });
        }

        // ==================== 設定ダイアログ ====================
        if self.settings_open {
            let mut open = true;
            let mut dark = self.dark_mode;
            let mut top = self.always_on_top;
            let mut tab_mode = self.startup_tab_mode;
            let mut confirm = self.confirm_kill_setting;
            let mut replace = self.replace_taskmgr;
            let mut tray_icon = self.tray_icon;
            let mut close_to_tray = self.close_to_tray;
            let mut minimize_to_tray = self.minimize_to_tray;
            let mut tray_usage_tip = self.tray_usage_tip;
            let mut interval = self.interval;
            let mut anim = self.graph_animation;
            let mut spline = self.graph_spline;
            let mut page = self.settings_page;
            let mut dark_colors = self.saved_settings.dark_colors;
            let mut light_colors = self.saved_settings.light_colors;
            let ifeo_active = integration::is_replace_active();

            egui::Window::new("設定")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .order(egui::Order::Foreground)
                .default_width(620.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut page, 0, "外観");
                        ui.selectable_value(&mut page, 1, "グラフと更新");
                        ui.selectable_value(&mut page, 2, "動作");
                        ui.selectable_value(&mut page, 3, "システム統合");
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| match page {
                        0 => {
                            ui.label(RichText::new("テーマ").strong());
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut dark, true, "ダークモード");
                                ui.radio_value(&mut dark, false, "ホワイトモード");
                            });
                            ui.add_space(8.0);
                            ui.label(RichText::new("配色").strong());
                            let defaults = if dark {
                                Palette::dark_default()
                            } else {
                                Palette::light_default()
                            };
                            for i in 0..12 {
                                let mut color = if dark {
                                    dark_colors.as_array()[i]
                                } else {
                                    light_colors.as_array()[i]
                                };
                                ui.horizontal(|ui| {
                                    if ui.color_edit_button_srgba(&mut color).changed() {
                                        if dark {
                                            dark_colors.set(i, color);
                                        } else {
                                            light_colors.set(i, color);
                                        }
                                    }
                                    ui.label(Palette::names()[i]);
                                    if ui.small_button("既定").clicked() {
                                        let c = defaults.as_array()[i];
                                        if dark {
                                            dark_colors.set(i, c);
                                        } else {
                                            light_colors.set(i, c);
                                        }
                                    }
                                });
                            }
                            if ui.button("すべて既定に戻す").clicked() {
                                dark_colors = Palette::dark_default();
                                light_colors = Palette::light_default();
                            }
                        }
                        1 => {
                            ui.label(RichText::new("更新の速度").strong());
                            for (label, value) in [
                                ("0.5 秒", 0.5_f32),
                                ("1 秒 (通常)", 1.0),
                                ("2 秒", 2.0),
                                ("5 秒 (低)", 5.0),
                            ] {
                                ui.radio_value(&mut interval, value, label);
                            }
                            ui.add_space(8.0);
                            ui.checkbox(&mut anim, "なめらかなアニメーション補間");
                            ui.checkbox(&mut spline, "スプライン補間 (滑らかな曲線)");
                        }
                        2 => {
                            ui.checkbox(&mut top, "常に手前に表示");
                            ui.label(RichText::new("起動時に開くタブ").strong());
                            ui.radio_value(&mut tab_mode, 0, "前回のタブを復元");
                            ui.radio_value(&mut tab_mode, 1, "常にプロセス");
                            ui.radio_value(&mut tab_mode, 2, "常にパフォーマンス");
                            ui.add_space(8.0);
                            ui.checkbox(&mut confirm, "タスク終了時に確認ダイアログを表示");
                            ui.checkbox(&mut minimize_to_tray, "最小化ボタンでもトレイに格納する")
                                .on_hover_text("タスクトレイアイコンが有効な場合に限ります");
                        }
                        _ => {
                            ui.checkbox(&mut replace, "タスクマネージャーの代わりにMiniTaskを起動")
                                .on_hover_text(
                                    "Ctrl+Shift+Esc やタスクバーからタスクマネージャーを開くと MiniTask が起動します",
                                );
                            ui.label(
                                RichText::new(format!(
                                    "現在の状態: {}",
                                    if ifeo_active { "有効" } else { "無効" }
                                ))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                            ui.add_space(8.0);
                            ui.checkbox(&mut tray_icon, "タスクトレイにアイコンを常駐する");
                            ui.checkbox(&mut close_to_tray, "閉じるボタンでトレイに格納する")
                                .on_hover_text("有効時、×ボタンは終了せずトレイへ格納します。完全終了はトレイアイコンの右クリックメニューから行います。");
                            ui.checkbox(
                                &mut tray_usage_tip,
                                "トレイアイコンのツールチップに CPU/メモリ使用率を表示",
                            );
                            ui.label(
                                RichText::new(format!(
                                    "トレイアイコン: {}",
                                    if self.tray_handle.is_some() { "有効" } else { "無効" }
                                ))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(
                                    "システム統合の切替には管理者権限が必要です。",
                                )
                                .small()
                                .color(self.palette.warn),
                            );
                            ui.separator();
                            ui.label(RichText::new("アップデート").strong());
                            ui.label(
                                RichText::new(format!(
                                    "現在のバージョン: {}",
                                    env!("CARGO_PKG_VERSION")
                                ))
                                .small()
                                .color(ui.visuals().weak_text_color()),
                            );
                            if self.update_busy {
                                ui.label("確認中 / ダウンロード中...");
                            } else if let Some(info) = self.update_info.clone() {
                                ui.label(
                                    RichText::new(format!("{} が利用可能", info.version))
                                        .color(self.palette.warn),
                                );
                                if ui.button("更新してインストール").clicked() {
                                    self.update_busy = true;
                                    let tx = self.update_tx.clone();
                                    std::thread::spawn(move || {
                                        let _ = tx.send(UpdateMsg::Download(
                                            crate::update::download(&info),
                                        ));
                                    });
                                }
                            } else if ui.button("今すぐ更新を確認").clicked() {
                                self.update_busy = true;
                                let tx = self.update_tx.clone();
                                std::thread::spawn(move || {
                                    let _ = tx.send(UpdateMsg::Check(crate::update::check_latest()));
                                });
                            }
                            if let Some(msg) = &self.update_msg {
                                ui.label(
                                    RichText::new(msg)
                                        .small()
                                        .color(ui.visuals().weak_text_color()),
                                );
                            }
                        }
                    });
                });

            self.settings_open = open;
            self.settings_page = page;
            if dark != self.dark_mode
                || dark_colors != self.saved_settings.dark_colors
                || light_colors != self.saved_settings.light_colors
            {
                self.theme_dirty = true;
            }
            self.dark_mode = dark;
            self.saved_settings.dark_colors = dark_colors;
            self.saved_settings.light_colors = light_colors;
            self.palette = if dark { dark_colors } else { light_colors };
            self.interval = interval;
            self.graph_animation = anim;
            self.graph_spline = spline;
            self.always_on_top = top;
            self.startup_tab_mode = tab_mode;
            self.confirm_kill_setting = confirm;
            if top && self.theme_dirty == false {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::AlwaysOnTop,
                ));
            } else if !top {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::Normal,
                ));
            }
            if replace != self.replace_taskmgr {
                match std::env::current_exe() {
                    Ok(exe) => match integration::set_replace_taskmgr(
                        replace,
                        &exe.to_string_lossy(),
                    ) {
                        Ok(()) => {
                            self.replace_taskmgr = replace;
                            self.notify(if replace {
                                "タスクマネージャーの代わりに MiniTask を起動します"
                            } else {
                                "タスクマネージャーの置換を解除しました"
                            });
                        }
                        Err(e) => self.notify(format!("置換の切替に失敗: {e}")),
                    },
                    Err(e) => self.notify(e.to_string()),
                }
            }
            self.close_to_tray = close_to_tray;
            self.minimize_to_tray = minimize_to_tray;
            self.minimize_flag
                .store(minimize_to_tray && tray_icon, Ordering::Relaxed);
            if tray_usage_tip != self.tray_usage_tip {
                // 次回のツールチップ更新で即時に反映されるようにする
                self.last_tray_tip.clear();
            }
            self.tray_usage_tip = tray_usage_tip;
            if tray_icon != self.tray_icon {
                let exe = std::env::current_exe()
                    .map(|e| e.to_string_lossy().to_string())
                    .unwrap_or_default();
                if tray_icon {
                    let _ = integration::set_shell_menu(true, &exe);
                    let (h, rx) = crate::tray::start();
                    self.tray_handle = Some(h);
                    self.tray_rx = Some(rx);
                    self.notify("タスクトレイアイコンを有効にしました");
                } else {
                    if let Some(mut h) = self.tray_handle.take() {
                        h.stop();
                    }
                    self.tray_rx = None;
                    let _ = integration::set_shell_menu(false, &exe);
                    self.notify("タスクトレイアイコンを無効にしました");
                }
                self.tray_icon = tray_icon;
            }
        }

        // ==================== SMART 詳細ダイアログ ====================
        if let Some(id) = self.smart_dialog_disk.clone() {
            let disks = self.load.disks.clone();
            let smart = disks
                .iter()
                .find(|d| d.id == id)
                .and_then(|d| d.smart.clone());
            let mut open = true;
            let mut selected = Some(id.clone());
            let title = format!(
                "SMART 詳細 — {}",
                disks
                    .iter()
                    .find(|d| d.id == id)
                    .map(|d| d.label.as_str())
                    .unwrap_or("ディスク")
            );
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .order(egui::Order::Foreground)
                .default_size([640.0, 520.0])
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    // 項目が多い場合もはみ出さないようスクロール可能にする
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        // ダイアログ内で別ディスクへ切り替え
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("ディスク").strong());
                            let current = selected
                                .as_ref()
                                .and_then(|id| disks.iter().find(|d| &d.id == id))
                                .map(|d| d.label.clone())
                                .unwrap_or_else(|| "—".to_owned());
                            egui::ComboBox::from_id_salt("smart_disk_select")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for d in &disks {
                                        ui.selectable_value(
                                            &mut selected,
                                            Some(d.id.clone()),
                                            d.label.as_str(),
                                        );
                                    }
                                });
                        });
                        ui.separator();
                    if let Some(smart) = &smart {
                        // 健康状態・温度を大きく強調
                        let health_color = match smart.status {
                            crate::smart::SmartStatus::Good => self.palette.mem,
                            crate::smart::SmartStatus::Caution => self.palette.warn,
                            crate::smart::SmartStatus::Bad => self.palette.danger,
                            crate::smart::SmartStatus::Unknown => ui.visuals().weak_text_color(),
                        };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("健康状態").strong());
                            ui.label(RichText::new(smart.status_text()).size(22.0).strong().color(health_color));
                            ui.add_space(24.0);
                            ui.label(RichText::new("温度").strong());
                            if let Some(t) = smart.temperature_c {
                                let temp_color = if t >= 70 {
                                    self.palette.danger
                                } else if t >= 60 {
                                    self.palette.warn
                                } else {
                                    self.palette.cpu
                                };
                                ui.label(RichText::new(format!("{t} °C")).size(22.0).strong().color(temp_color));
                            } else {
                                ui.label(RichText::new("—").size(22.0).strong());
                            }
                        });
                        ui.separator();
                        for (label, value) in [
                            ("モデル", smart.model.as_str()),
                            ("シリアル番号", smart.serial.as_str()),
                            ("ファームウェア", smart.firmware.as_str()),
                            ("インターフェース", smart.interface.as_str()),
                            (
                                "温度",
                                smart
                                    .temperature_c
                                    .map(|t| format!("{t} °C"))
                                    .as_deref()
                                    .unwrap_or("—"),
                            ),
                            (
                                "電源投入時間",
                                smart
                                    .power_on_hours
                                    .map(|h| format!("{h} 時間"))
                                    .as_deref()
                                    .unwrap_or("—"),
                            ),
                            (
                                "電源投入回数",
                                smart
                                    .power_cycles
                                    .map(|c| c.to_string())
                                    .as_deref()
                                    .unwrap_or("—"),
                            ),
                        ] {
                            Self::detail_row(ui, label, value.to_owned());
                        }
                        if !smart.extra.is_empty() {
                            ui.add_space(8.0);
                            ui.label(RichText::new("追加情報").strong());
                            for (k, v) in &smart.extra {
                                Self::detail_row(ui, k, v.clone());
                            }
                        }
                        if !smart.attributes.is_empty() {
                            ui.add_space(8.0);
                            ui.label(RichText::new("SMART 属性").strong());
                            egui_extras::TableBuilder::new(ui)
                                .striped(true)
                                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                                .column(Column::initial(50.0))
                                .column(Column::initial(150.0))
                                .column(Column::initial(60.0))
                                .column(Column::initial(60.0))
                                .column(Column::initial(60.0))
                                .column(Column::remainder())
                                .header(22.0, |mut h| {
                                    for name in ["ID", "属性名", "現在値", "最悪値", "しきい値", "生の値"] {
                                        h.col(|ui| {
                                            ui.strong(name);
                                        });
                                    }
                                })
                                .body(|mut body| {
                                    for a in &smart.attributes {
                                        body.row(20.0, |mut row| {
                                            let bad = a.threshold > 0 && a.current <= a.threshold;
                                            let color = if bad {
                                                Some(self.palette.danger)
                                            } else {
                                                None
                                            };
                                            let cells = [
                                                format!("{:02X}", a.id),
                                                a.name.clone(),
                                                a.current.to_string(),
                                                a.worst.to_string(),
                                                a.threshold.to_string(),
                                                a.raw_text.clone(),
                                            ];
                                            for cell in cells {
                                                row.col(|ui| {
                                                    let mut rt = RichText::new(cell.clone()).size(12.0);
                                                    if let Some(c) = color {
                                                        rt = rt.color(c);
                                                    }
                                                    ui.label(rt);
                                                });
                                            }
                                        });
                                    }
                                });
                        }
                    } else {
                        ui.label("このディスクの SMART 情報を取得できませんでした。");
                    }
                    });
                });
            // ディスクが切断された場合もダイアログを閉じる
            let valid = selected
                .as_ref()
                .map(|id| disks.iter().any(|d| &d.id == id))
                .unwrap_or(false);
            let next = if open && valid { selected } else { None };
            if let Some(sid) = &next {
                if sid != &id {
                    // ダイアログ内でディスクを切り替えたら即時再取得
                    self.refresh_smart(sid);
                }
            }
            self.smart_dialog_disk = next;
        }

    }
}

/// エクスプローラーでファイルの場所を開く。
fn open_location(exe: &str) {
    use std::os::windows::process::CommandExt;
    if exe.is_empty() {
        return;
    }
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{exe}\""))
        .spawn();
}
