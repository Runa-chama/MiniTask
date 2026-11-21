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
            self.load = load;
            self.cpu_hist.push_back(load.cpu);
            let mem_pct = if load.mem_total > 0 {
                load.mem_used as f32 / load.mem_total as f32 * 100.0
            } else {
                0.0
            };
            self.mem_hist.push_back(mem_pct);
            self.rom_hist.push_back(load.rom);
            self.gpu_hist.push_back(load.gpu);
            self.npu_hist.push_back(load.npu);
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

    /// 1 メトリクスのグラフ行 (左: 値 / 右: 折れ線)。
    fn perf_graph(
        ui: &mut egui::Ui,
        title: &str,
        value: f32,
        hist: &VecDeque<f32>,
        color: Color32,
        subtitle: &str,
    ) {
        ui.add_space(4.0);
        ui.group(|ui| {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(180.0, 0.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.label(RichText::new(title).strong().size(15.0).color(color));
                        ui.label(
                            RichText::new(format!("{value:.0}%"))
                                .strong()
                                .monospace()
                                .size(27.0)
                                .color(color),
                        );
                        ui.label(
                            RichText::new(subtitle)
                                .small()
                                .color(Color32::from_gray(140)),
                        );
                    },
                );
                let width = (ui.available_width() - 8.0).max(120.0);
                util::sparkline(ui, hist, 100.0, color, Vec2::new(width, 84.0));
            });
        });
    }

    fn performance_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("システム全体の使用率").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new("過去 120 サンプル / 0〜100%")
                        .size(12.0)
                        .color(Color32::from_gray(120)),
                );
            });
        });
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mem_pct = if self.load.mem_total > 0 {
                self.load.mem_used as f32 / self.load.mem_total as f32 * 100.0
            } else {
                0.0
            };
            let total_gb = self.load.mem_total as f64 / (1024.0 * 1024.0 * 1024.0);
            let used_gb = self.load.mem_used as f64 / (1024.0 * 1024.0 * 1024.0);

            Self::perf_graph(ui, "CPU", self.load.cpu, &self.cpu_hist, CPU_COLOR, "プロセッサ");
            Self::perf_graph(
                ui,
                "RAM (メモリ)",
                mem_pct,
                &self.mem_hist,
                MEM_COLOR,
                &format!("{used_gb:.1} / {total_gb:.1} GB"),
            );
            Self::perf_graph(
                ui,
                "ROM (ストレージ)",
                self.load.rom,
                &self.rom_hist,
                ROM_COLOR,
                "C: ドライブ使用率",
            );
            Self::perf_graph(
                ui,
                "GPU",
                self.load.gpu,
                &self.gpu_hist,
                GPU_COLOR,
                "グラフィックエンジン",
            );
            let npu_sub = if self.load.npu_available {
                "NPU エンジン"
            } else {
                "NPU 非搭載 / 取得不可"
            };
            Self::perf_graph(ui, "NPU", self.load.npu, &self.npu_hist, NPU_COLOR, npu_sub);
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
