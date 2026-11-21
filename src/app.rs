//! アプリ本体: 状態管理と UI 描画。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Vec2};
use egui_extras::{Column, TableBuilder};

use crate::net::NetSession;
use crate::process::{self, ProcCollector, ProcRow, SysLoad};
use crate::startup::{self, StartupEntry};
use crate::util::{
    self, ACCENT, CPU_COLOR, DANGER, GPU_COLOR, MEM_COLOR, NPU_COLOR, ROM_COLOR, WARN,
};

const HISTORY_LEN: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Processes,
    Performance,
    Startup,
}

/// パフォーマンスタブで選択中のメトリクス。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PerfSel {
    Cpu,
    Ram,
    Rom,
    Gpu,
    Npu,
}

/// プロセス一覧の列。タブの右クリックメニューで表示/非表示を切り替える。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Name,
    Pid,
    Cpu,
    Mem,
    NetRecv,
    NetSent,
    IoRead,
    IoWrite,
    User,
    Start,
    Exe,
    Cmd,
}

impl ColumnKind {
    const ALL: [Self; 12] = [
        Self::Name,
        Self::Pid,
        Self::Cpu,
        Self::Mem,
        Self::NetRecv,
        Self::NetSent,
        Self::IoRead,
        Self::IoWrite,
        Self::User,
        Self::Start,
        Self::Exe,
        Self::Cmd,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Name => "名前",
            Self::Pid => "PID",
            Self::Cpu => "CPU",
            Self::Mem => "メモリ",
            Self::NetRecv => "ネット受信",
            Self::NetSent => "ネット送信",
            Self::IoRead => "I/O 読取",
            Self::IoWrite => "I/O 書込",
            Self::User => "ユーザー",
            Self::Start => "開始時刻",
            Self::Exe => "パス",
            Self::Cmd => "コマンドライン",
        }
    }

    fn is_numeric(self) -> bool {
        !matches!(self, Self::Name | Self::User | Self::Exe | Self::Cmd)
    }

    fn spec(self) -> Column {
        match self {
            Self::Name => Column::auto().at_least(180.0).clip(true),
            Self::Pid => Column::initial(60.0).clip(true),
            Self::Cpu => Column::initial(64.0).clip(true),
            Self::Mem => Column::initial(88.0).clip(true),
            Self::NetRecv => Column::initial(92.0).clip(true),
            Self::NetSent => Column::initial(92.0).clip(true),
            Self::IoRead => Column::initial(88.0).clip(true),
            Self::IoWrite => Column::initial(88.0).clip(true),
            Self::User => Column::initial(110.0).clip(true),
            Self::Start => Column::initial(126.0).clip(true),
            Self::Exe => Column::auto().at_least(220.0).clip(true),
            Self::Cmd => Column::auto().at_least(260.0).clip(true),
        }
    }

    fn render(self, ui: &mut egui::Ui, row: &ProcRow) {
        match self {
            Self::Name => {
                ui.label(&row.name);
            }
            Self::Pid => util::num_cell(ui, row.pid.to_string(), None),
            Self::Cpu => util::num_cell(ui, format!("{:.1}%", row.cpu), util::cpu_color(row.cpu)),
            Self::Mem => util::num_cell(ui, util::fmt_bytes(row.mem), None),
            Self::NetRecv => util::num_cell(
                ui,
                util::fmt_rate(row.net_recv),
                (row.net_recv > 0).then_some(MEM_COLOR),
            ),
            Self::NetSent => util::num_cell(
                ui,
                util::fmt_rate(row.net_sent),
                (row.net_sent > 0).then_some(CPU_COLOR),
            ),
            Self::IoRead => util::num_cell(ui, util::fmt_rate(row.io_read), None),
            Self::IoWrite => util::num_cell(ui, util::fmt_rate(row.io_write), None),
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
    rom_hist: VecDeque<f32>,
    gpu_hist: VecDeque<f32>,
    npu_hist: VecDeque<f32>,
    net: Option<NetSession>,
    net_err: Option<String>,

    tab: Tab,
    perf_sel: PerfSel,
    search: String,
    sort_col: ColumnKind,
    sort_desc: bool,
    selected: Option<u32>,
    columns: [bool; 12],
    interval: f32,
    last_refresh: Instant,

    confirm_kill: Option<(u32, String)>,
    toast: Option<(String, Instant)>,

    startup_entries: Vec<StartupEntry>,
    confirm_delete: Option<StartupEntry>,
}

impl MiniTaskApp {
    pub fn new() -> Self {
        let collector = ProcCollector::new().ok();
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
            rom_hist: VecDeque::new(),
            gpu_hist: VecDeque::new(),
            npu_hist: VecDeque::new(),
            net,
            net_err,
            tab: if std::env::var("MINITASK_TAB").as_deref() == Ok("performance") {
                Tab::Performance
            } else {
                Tab::Processes
            },
            perf_sel: PerfSel::Cpu,
            search: String::new(),
            sort_col: ColumnKind::Cpu,
            sort_desc: true,
            selected: None,
            // 既定は基本8列のみ。詳細列はタブの右クリックで有効化する。
            columns: [true, true, true, true, true, true, true, true, false, false, false, false],
            interval: 1.0,
            last_refresh: Instant::now() - Duration::from_secs(10),
            confirm_kill: None,
            toast: None,
            startup_entries: startup::list(),
            confirm_delete: None,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if let Some(collector) = &mut self.collector {
            let (rows, load) = collector.refresh();
            self.rows = rows;
            let mem_pct = if load.mem_total > 0 {
                load.mem_used as f32 / load.mem_total as f32 * 100.0
            } else {
                0.0
            };
            self.cpu_hist.push_back(load.cpu);
            self.mem_hist.push_back(mem_pct);
            self.rom_hist.push_back(load.rom);
            self.gpu_hist.push_back(load.gpu);
            self.npu_hist.push_back(load.npu);
            self.load = load;
            for hist in [
                &mut self.cpu_hist,
                &mut self.mem_hist,
                &mut self.rom_hist,
                &mut self.gpu_hist,
                &mut self.npu_hist,
            ] {
                while hist.len() > HISTORY_LEN {
                    hist.pop_front();
                }
            }
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

    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn ask_kill(&mut self, pid: u32) {
        if let Some(row) = self.rows.iter().find(|r| r.pid == pid) {
            self.confirm_kill = Some((pid, row.name.clone()));
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
                ColumnKind::Pid => a.pid.cmp(&b.pid),
                ColumnKind::Cpu => a.cpu.total_cmp(&b.cpu),
                ColumnKind::Mem => a.mem.cmp(&b.mem),
                ColumnKind::NetRecv => a.net_recv.cmp(&b.net_recv),
                ColumnKind::NetSent => a.net_sent.cmp(&b.net_sent),
                ColumnKind::IoRead => a.io_read.cmp(&b.io_read),
                ColumnKind::IoWrite => a.io_write.cmp(&b.io_write),
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
        if ui.button(text).clicked() {
            if self.sort_col == col {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort_col = col;
                self.sort_desc = col.is_numeric();
            }
        }
    }
}

impl eframe::App for MiniTaskApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let interval = Duration::from_secs_f32(self.interval);
        let elapsed = self.last_refresh.elapsed();
        if elapsed >= interval {
            self.refresh();
        }
        ctx.request_repaint_after(interval.saturating_sub(elapsed));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
                let processes = ui.selectable_value(&mut self.tab, Tab::Processes, "  プロセス  ");
                processes.context_menu(|ui| self.column_menu(ui));
                ui.selectable_value(&mut self.tab, Tab::Performance, "  パフォーマンス  ");
                ui.selectable_value(&mut self.tab, Tab::Startup, "  スタートアップ  ");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new("プロセスタブを右クリックで列を選択")
                            .size(12.0)
                            .color(Color32::from_gray(120)),
                    );
                });
            });
            ui.separator();
            match self.tab {
                Tab::Processes => self.processes_tab(ui),
                Tab::Performance => self.performance_tab(ui),
                Tab::Startup => self.startup_tab(ui),
            }
        });

        self.dialogs(ui.ctx());
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(mut session) = self.net.take() {
            session.stop();
        }
    }
}

impl MiniTaskApp {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("MiniTask").size(20.0).strong().color(ACCENT));
            ui.label(
                RichText::new("軽量タスクマネージャー")
                    .size(12.0)
                    .color(Color32::from_gray(140)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let total_gb = self.load.mem_total as f64 / (1024.0 * 1024.0 * 1024.0);
                let used_gb = self.load.mem_used as f64 / (1024.0 * 1024.0 * 1024.0);
                let mem_pct = if self.load.mem_total > 0 {
                    self.load.mem_used as f32 / self.load.mem_total as f32 * 100.0
                } else {
                    0.0
                };
                ui.label(
                    RichText::new(format!("メモリ {used_gb:.1}/{total_gb:.1} GB ({mem_pct:.0}%)"))
                        .monospace()
                        .size(12.0),
                );
                util::sparkline(ui, &self.mem_hist, 100.0, MEM_COLOR, Vec2::new(110.0, 26.0));
                ui.add_space(14.0);
                ui.label(
                    RichText::new(format!("CPU {:>3.0}%", self.load.cpu))
                        .monospace()
                        .size(12.0),
                );
                util::sparkline(ui, &self.cpu_hist, 100.0, CPU_COLOR, Vec2::new(110.0, 26.0));
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
                            table_row.col(|ui| c.render(ui, &row));
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
                                self.confirm_kill = Some((row.pid, row.name.clone()));
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

    /// パフォーマンスタブの左ナビ 1 項目 (クリックでメトリクス選択)。
    fn perf_nav_item(
        ui: &mut egui::Ui,
        label: &str,
        pct: f32,
        color: Color32,
        selected: bool,
    ) -> egui::Response {
        let width = ui.available_width().clamp(80.0, 300.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 46.0), egui::Sense::click());
        let bg = if selected {
            Color32::from_rgb(45, 49, 60)
        } else if resp.hovered() {
            Color32::from_rgb(38, 42, 52)
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 5.0, bg);
        }
        if selected {
            let bar = egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.top() + 10.0),
                egui::pos2(rect.left() + 3.5, rect.bottom() - 10.0),
            );
            ui.painter().rect_filled(bar, 1.5, color);
        }
        ui.painter().text(
            egui::pos2(rect.left() + 14.0, rect.top() + 7.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );
        ui.painter().text(
            egui::pos2(rect.right() - 10.0, rect.top() + 5.0),
            egui::Align2::RIGHT_TOP,
            format!("{pct:.0}%"),
            egui::FontId::proportional(17.0),
            color,
        );
        ui.add_space(2.0);
        resp
    }

    /// 詳細情報の 1 行 (ラベル左・値右)。
    fn detail_row(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(label)
                    .size(12.0)
                    .color(Color32::from_gray(150)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(value).size(12.0).monospace());
            });
        });
    }

    fn performance_tab(&mut self, ui: &mut egui::Ui) {
        let mem_pct = if self.load.mem_total > 0 {
            self.load.mem_used as f32 / self.load.mem_total as f32 * 100.0
        } else {
            0.0
        };

        // --- 左ナビ: メトリクス選択 (タスクマネージャー風) ---
        egui::Panel::left("perf_nav")
            .exact_size(190.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                let items = [
                    (PerfSel::Cpu, "CPU", self.load.cpu, CPU_COLOR),
                    (PerfSel::Ram, "メモリ", mem_pct, MEM_COLOR),
                    (PerfSel::Rom, "ROM (C:)", self.load.rom, ROM_COLOR),
                    (PerfSel::Gpu, "GPU", self.load.gpu, GPU_COLOR),
                    (PerfSel::Npu, "NPU", self.load.npu, NPU_COLOR),
                ];
                for (sel, label, pct, color) in items {
                    let resp = Self::perf_nav_item(ui, label, pct, color, self.perf_sel == sel);
                    if resp.clicked() {
                        self.perf_sel = sel;
                    }
                }
            });

        // --- 右サイド: 各メトリクスの簡易グラフ ---
        egui::Panel::right("perf_minis")
            .exact_size(215.0)
            .show(ui, |ui| {
                ui.add_space(2.0);
                let w = ui.available_width().clamp(80.0, 240.0);
                let minis: [(&str, &VecDeque<f32>, f32, Color32); 5] = [
                    ("CPU", &self.cpu_hist, self.load.cpu, CPU_COLOR),
                    ("メモリ", &self.mem_hist, mem_pct, MEM_COLOR),
                    ("ROM (C:)", &self.rom_hist, self.load.rom, ROM_COLOR),
                    ("GPU", &self.gpu_hist, self.load.gpu, GPU_COLOR),
                    ("NPU", &self.npu_hist, self.load.npu, NPU_COLOR),
                ];
                for (name, hist, pct, color) in minis {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(name).size(12.0).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("{pct:.0}%"))
                                    .size(12.0)
                                    .monospace()
                                    .color(color),
                            );
                        });
                    });
                    util::sparkline(ui, hist, 100.0, color, Vec2::new(w, 36.0));
                    ui.add_space(7.0);
                }
            });

        // --- 中央: 選択中メトリクスの大きなグラフと詳細 ---
        let (title, subtitle, value, hist, color) = match self.perf_sel {
            PerfSel::Cpu => (
                "CPU",
                if self.load.cpu_brand.is_empty() {
                    "プロセッサ".to_owned()
                } else {
                    self.load.cpu_brand.clone()
                },
                format!("{:.0}%", self.load.cpu),
                &self.cpu_hist,
                CPU_COLOR,
            ),
            PerfSel::Ram => (
                "メモリ",
                "物理メモリ (RAM)".to_owned(),
                format!("{mem_pct:.0}%"),
                &self.mem_hist,
                MEM_COLOR,
            ),
            PerfSel::Rom => (
                "ROM (ストレージ)",
                "C: ドライブ".to_owned(),
                format!("{:.0}%", self.load.rom),
                &self.rom_hist,
                ROM_COLOR,
            ),
            PerfSel::Gpu => (
                "GPU",
                self.load.gpu_model.clone(),
                format!("{:.0}%", self.load.gpu),
                &self.gpu_hist,
                GPU_COLOR,
            ),
            PerfSel::Npu => (
                "NPU",
                if self.load.npu_available {
                    "NPU エンジン".to_owned()
                } else {
                    "NPU 非搭載 / 取得不可".to_owned()
                },
                format!("{:.0}%", self.load.npu),
                &self.npu_hist,
                NPU_COLOR,
            ),
        };

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(title).size(22.0).strong().color(color));
                ui.label(
                    RichText::new(subtitle)
                        .size(12.0)
                        .color(Color32::from_gray(140)),
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
        ui.add_space(4.0);
        let gh = (ui.available_height() * 0.56).clamp(140.0, 340.0);
        let gw = ui.available_width().clamp(200.0, 4000.0);
        util::perf_chart(ui, hist, 100.0, color, Vec2::new(gw, gh));
        ui.add_space(6.0);
        ui.separator();

        // 詳細情報 (2 列)
        let d = &self.load;
        let gb = util::fmt_bytes;
        ui.columns(2, |cols| {
            let (l, rest) = cols.split_first_mut().unwrap();
            let r = rest.first_mut().unwrap();
            l.vertical(|ui| match self.perf_sel {
                PerfSel::Cpu => {
                    Self::detail_row(ui, "利用率", format!("{:.1}%", d.cpu));
                    Self::detail_row(
                        ui,
                        "速度",
                        format!("{:.2} GHz", d.cpu_freq_mhz as f64 / 1000.0),
                    );
                    Self::detail_row(ui, "プロセス", d.processes.to_string());
                    Self::detail_row(ui, "ハンドル", d.handles.to_string());
                    Self::detail_row(ui, "論理プロセッサ", d.logical_cores.to_string());
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
                }
                PerfSel::Rom => {
                    Self::detail_row(ui, "利用率", format!("{:.1}%", d.rom));
                    Self::detail_row(
                        ui,
                        "容量",
                        format!("{} / {}", gb(d.rom_total - d.rom_free), gb(d.rom_total)),
                    );
                    Self::detail_row(ui, "空き容量", gb(d.rom_free));
                    Self::detail_row(ui, "読み取り速度", util::fmt_rate(d.rom_read));
                }
                PerfSel::Gpu => {
                    Self::detail_row(ui, "使用率", format!("{:.1}%", d.gpu));
                    Self::detail_row(ui, "ビデオメモリ使用", gb(d.gpu_mem_used));
                    Self::detail_row(ui, "ビデオメモリ合計", gb(d.gpu_mem_total));
                }
                PerfSel::Npu => {
                    Self::detail_row(ui, "使用率", format!("{:.1}%", d.npu));
                    Self::detail_row(
                        ui,
                        "状態",
                        if d.npu_available { "計測中" } else { "取得不可" },
                    );
                }
            });
            r.vertical(|ui| match self.perf_sel {
                PerfSel::Cpu => {
                    Self::detail_row(ui, "スレッド", d.threads.to_string());
                    let s = d.uptime_secs;
                    Self::detail_row(
                        ui,
                        "稼働時間",
                        format!("{}:{:02}:{:02}:{:02}", s / 86400, s / 3600 % 24, s / 60 % 60, s % 60),
                    );
                    Self::detail_row(ui, "更新間隔", format!("{} 秒", self.interval));
                }
                PerfSel::Ram => {
                    Self::detail_row(ui, "合計 (物理)", gb(d.mem_total));
                    Self::detail_row(ui, "プロセス数", d.processes.to_string());
                }
                PerfSel::Rom => {
                    Self::detail_row(ui, "書き込み速度", util::fmt_rate(d.rom_written));
                }
                PerfSel::Gpu => {
                    Self::detail_row(ui, "モデル", d.gpu_model.clone());
                }
                PerfSel::Npu => {
                    Self::detail_row(
                        ui,
                        "カウンタ",
                        if d.npu_available { "NPU Engine" } else { "—" },
                    );
                }
            });
        });
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
                        .color(Color32::from_gray(140)),
                );
            });
        });
        ui.add_space(2.0);

        let entries = self.startup_entries.clone();
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
                            ui.label(&entry.name);
                        });
                        row.col(|ui| {
                            let label = if entry.enabled { "有効" } else { "無効" };
                            let color = if entry.enabled { MEM_COLOR } else { Color32::from_gray(140) };
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
                            .color(MEM_COLOR),
                    );
                }
                Some(_) => {
                    ui.label(
                        RichText::new("ネットワーク計測: セッション停止")
                            .size(12.0)
                            .color(WARN),
                    );
                }
                None => {
                    if let Some(err) = &self.net_err {
                        ui.label(
                            RichText::new(format!("ネットワーク計測: 無効 — {err}"))
                                .size(12.0)
                                .color(WARN),
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
                    ui.label(RichText::new(msg).size(12.0).color(ACCENT));
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some((pid, name)) = self.confirm_kill.clone() {
            egui::Window::new("タスクの終了")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(format!("「{name}」(PID {pid}) を強制終了しますか？"));
                    ui.label(
                        RichText::new("保存されていないデータは失われます。")
                            .small()
                            .color(WARN),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("終了する").color(DANGER))
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

        if let Some(entry) = self.confirm_delete.clone() {
            egui::Window::new("スタートアップから削除")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(340.0);
                    ui.label(format!("「{}」をスタートアップから削除しますか？", entry.name));
                    ui.label(
                        RichText::new("レジストリ値またはファイルが削除されます。")
                            .small()
                            .color(WARN),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("削除する").color(DANGER))
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
