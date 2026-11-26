//! Docked studio interface (`egui_dock`) implementing the Tool Suite Design Language for Xbox 360 -> PC asset
//! inspection and batch conversion.

use std::path::PathBuf;

use eframe::egui::{self, RichText};
use egui_dock::{DockArea, DockState, NodeIndex, TabViewer};
use x360_xenos_texture::XenosFormat;

use crate::pipeline::{EngineProfile, Inspection, JobKind, JobStatus, QueueItem, inspect_file, run_job};
use crate::task::Task;
use crate::theme::{self, ThemeId};

/// Docked panels in `xenolith`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Tab {
    /// Input browser & manual conversion settings.
    Browser,
    /// Asset inspector, disassembly & HLSL preview.
    Inspector,
    /// Batch conversion queue.
    Queue,
    /// Conversion log & errors.
    Log,
}

impl Tab {
    const fn title(self) -> &'static str {
        match self {
            Self::Browser => "Asset Browser",
            Self::Inspector => "Inspector & Preview",
            Self::Queue => "Conversion Queue",
            Self::Log => "Log & Problems",
        }
    }
}

/// Default dock layout: Browser left, Inspector center, Queue right, Log bottom.
#[must_use]
pub fn default_layout() -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Inspector]);
    let s = dock.main_surface_mut();
    let [center, _left] = s.split_left(NodeIndex::root(), 0.24, vec![Tab::Browser]);
    let [center, _right] = s.split_right(center, 0.72, vec![Tab::Queue]);
    let [_center, _bottom] = s.split_below(center, 0.74, vec![Tab::Log]);
    dock
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Info,
    Ok,
    Error,
}

#[derive(Debug, Clone)]
struct LogEntry {
    level: LogLevel,
    message: String,
}

/// Main `xenolith` application state.
pub struct StudioApp {
    theme: ThemeId,
    applied_theme: Option<ThemeId>,
    engine: EngineProfile,
    dock: DockState<Tab>,
    input_path: String,
    output_dir: String,
    raw_width: usize,
    raw_height: usize,
    raw_mips: usize,
    raw_slices: usize,
    raw_format: XenosFormat,
    inspection: Option<Inspection>,
    queue: Vec<QueueItem>,
    logs: Vec<LogEntry>,
    active_inspect: Option<Task<Inspection>>,
    active_batch: Option<Task<Vec<(usize, JobStatus)>>>,
}

impl Default for StudioApp {
    fn default() -> Self {
        Self {
            theme: ThemeId::Dark,
            applied_theme: None,
            engine: EngineProfile::default(),
            dock: default_layout(),
            input_path: String::new(),
            output_dir: "out".into(),
            raw_width: 512,
            raw_height: 512,
            raw_mips: 1,
            raw_slices: 1,
            raw_format: XenosFormat::Dxt5,
            inspection: None,
            queue: Vec::new(),
            logs: vec![LogEntry {
                level: LogLevel::Info,
                message: "Xenolith ready. Enter a file path to inspect or queue for conversion.".into(),
            }],
            active_inspect: None,
            active_batch: None,
        }
    }
}

impl StudioApp {
    fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.logs.push(LogEntry {
            level,
            message: message.into(),
        });
    }

    fn poll_tasks(&mut self) {
        if let Some(res) = self.active_inspect.as_ref().and_then(Task::poll) {
            self.active_inspect = None;
            match res {
                Ok(insp) => {
                    self.log(
                        LogLevel::Ok,
                        format!("Inspected {} ({})", insp.path.display(), insp.detected),
                    );
                    self.inspection = Some(insp);
                }
                Err(e) => self.log(LogLevel::Error, e),
            }
        }
        if let Some(res) = self.active_batch.as_ref().and_then(Task::poll) {
            self.active_batch = None;
            match res {
                Ok(updates) => {
                    let mut ok_count = 0usize;
                    let mut err_count = 0usize;
                    for (idx, status) in updates {
                        match &status {
                            JobStatus::Done { output, bytes } => {
                                ok_count += 1;
                                self.log(LogLevel::Ok, format!("Wrote {} ({bytes} bytes)", output.display()));
                            }
                            JobStatus::Failed(msg) => {
                                err_count += 1;
                                self.log(LogLevel::Error, msg.clone());
                            }
                            JobStatus::Pending => {}
                        }
                        if let Some(item) = self.queue.get_mut(idx) {
                            item.status = status;
                        }
                    }
                    self.log(
                        if err_count == 0 { LogLevel::Ok } else { LogLevel::Error },
                        format!("Batch finished: {ok_count} succeeded, {err_count} failed."),
                    );
                }
                Err(e) => self.log(LogLevel::Error, e),
            }
        }
    }

    fn start_inspect(&mut self) {
        let p = PathBuf::from(self.input_path.trim());
        if p.as_os_str().is_empty() {
            self.log(LogLevel::Error, "Enter an input file path first.");
            return;
        }
        let engine = self.engine;
        self.active_inspect = Some(Task::spawn("inspect", move || inspect_file(&p, engine)));
    }

    fn start_batch(&mut self) {
        if self.queue.is_empty() {
            self.log(LogLevel::Error, "Conversion queue is empty.");
            return;
        }
        let out_dir = PathBuf::from(self.output_dir.trim());
        let work: Vec<(usize, PathBuf, JobKind)> = self
            .queue
            .iter()
            .enumerate()
            .map(|(i, q)| (i, q.input.clone(), q.kind.clone()))
            .collect();
        self.active_batch = Some(Task::spawn("batch convert", move || {
            let mut results = Vec::with_capacity(work.len());
            for (idx, input, kind) in work {
                let st = match run_job(&input, &kind, &out_dir) {
                    Ok((output, bytes)) => JobStatus::Done { output, bytes },
                    Err(e) => JobStatus::Failed(e),
                };
                results.push((idx, st));
            }
            Ok(results)
        }));
    }
}

struct Viewer<'a> {
    app: &'a mut StudioApp,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        tab.title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        let pal = theme::tokens().palette(self.app.theme);
        match tab {
            Tab::Browser => {
                ui.heading("Input File");
                ui.add(egui::TextEdit::singleline(&mut self.app.input_path).hint_text("C:\\path\\to\\asset.bin"));
                ui.horizontal(|ui| {
                    if ui.button("Inspect").clicked() {
                        self.app.start_inspect();
                    }
                    if let Some(suggested) = self.app.inspection.as_ref().and_then(|i| i.suggested.clone())
                        && ui.button(format!("Queue ({})", suggested.label())).clicked()
                    {
                        self.app.queue.push(QueueItem {
                            input: PathBuf::from(self.app.input_path.trim()),
                            kind: suggested,
                            status: JobStatus::Pending,
                        });
                    }
                });
                ui.separator();
                ui.heading("Raw Xenos Texture Settings");
                egui::Grid::new("raw_xenos_grid").num_columns(2).show(ui, |ui| {
                    ui.label("Width");
                    ui.add(egui::DragValue::new(&mut self.app.raw_width).range(1..=16384));
                    ui.end_row();
                    ui.label("Height");
                    ui.add(egui::DragValue::new(&mut self.app.raw_height).range(1..=16384));
                    ui.end_row();
                    ui.label("Mips");
                    ui.add(egui::DragValue::new(&mut self.app.raw_mips).range(1..=15));
                    ui.end_row();
                    ui.label("Slices");
                    ui.add(egui::DragValue::new(&mut self.app.raw_slices).range(1..=6));
                    ui.end_row();
                    ui.label("Format");
                    egui::ComboBox::from_id_salt("raw_fmt")
                        .selected_text(format!("{:?}", self.app.raw_format))
                        .show_ui(ui, |ui| {
                            for fmt in [
                                XenosFormat::Dxt1,
                                XenosFormat::Dxt3,
                                XenosFormat::Dxt5,
                                XenosFormat::Dxt5a,
                                XenosFormat::Bc5,
                                XenosFormat::Rgb565,
                                XenosFormat::Argb8,
                                XenosFormat::L8,
                                XenosFormat::Rgba16f,
                            ] {
                                ui.selectable_value(&mut self.app.raw_format, fmt, format!("{fmt:?}"));
                            }
                        });
                    ui.end_row();
                });
                if ui.button("Queue as Raw Xenos Texture").clicked() && !self.app.input_path.trim().is_empty() {
                    self.app.queue.push(QueueItem {
                        input: PathBuf::from(self.app.input_path.trim()),
                        kind: JobKind::RawXenosTexture {
                            width: self.app.raw_width,
                            height: self.app.raw_height,
                            mips: self.app.raw_mips,
                            slices: self.app.raw_slices,
                            format: self.app.raw_format,
                        },
                        status: JobStatus::Pending,
                    });
                }
            }
            Tab::Inspector => {
                if let Some(ref insp) = self.app.inspection {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&insp.detected).strong().color(pal.accent_text));
                        ui.label(RichText::new(insp.path.display().to_string()).color(pal.ink_muted));
                    });
                    ui.separator();
                    egui::Grid::new("insp_fields")
                        .num_columns(2)
                        .striped(true)
                        .show(ui, |ui| {
                            for (k, v) in &insp.fields {
                                ui.label(RichText::new(k).color(pal.ink_muted));
                                ui.label(v);
                                ui.end_row();
                            }
                        });
                    ui.separator();
                    egui::ScrollArea::vertical().id_salt("insp_primary").show(ui, |ui| {
                        ui.monospace(&insp.primary_text);
                        if let Some(ref sec) = insp.secondary_text {
                            ui.separator();
                            ui.label(RichText::new("Translated HLSL").strong());
                            ui.monospace(sec);
                        }
                    });
                } else {
                    ui.label(RichText::new("No asset inspected yet.").color(pal.ink_muted));
                }
            }
            Tab::Queue => {
                ui.horizontal(|ui| {
                    ui.label("Output dir:");
                    ui.text_edit_singleline(&mut self.app.output_dir);
                });
                ui.horizontal(|ui| {
                    let running = self.app.active_batch.is_some();
                    if ui.add_enabled(!running, egui::Button::new("Run Batch")).clicked() {
                        self.app.start_batch();
                    }
                    if ui.button("Clear").clicked() {
                        self.app.queue.clear();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().id_salt("queue_scroll").show(ui, |ui| {
                    for item in &self.app.queue {
                        let (badge, col) = match &item.status {
                            JobStatus::Pending => ("PENDING", pal.ink_muted),
                            JobStatus::Done { .. } => ("DONE", pal.status_ok),
                            JobStatus::Failed(_) => ("FAILED", pal.status_error),
                        };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(badge).strong().color(col));
                            ui.label(item.kind.label());
                            ui.label(RichText::new(item.input.display().to_string()).color(pal.ink_muted));
                        });
                    }
                });
            }
            Tab::Log => {
                egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    for entry in &self.app.logs {
                        let col = match entry.level {
                            LogLevel::Info => pal.status_info,
                            LogLevel::Ok => pal.status_ok,
                            LogLevel::Error => pal.status_error,
                        };
                        ui.label(RichText::new(&entry.message).color(col));
                    }
                });
            }
        }
    }
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.applied_theme != Some(self.theme) {
            theme::apply(&ctx, self.theme);
            self.applied_theme = Some(self.theme);
        }
        self.poll_tasks();
        if self.active_inspect.is_some() || self.active_batch.is_some() {
            ctx.request_repaint();
        }
        let pal = theme::tokens().palette(self.theme);

        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Xenolith").strong().color(pal.ink));
                ui.separator();
                egui::ComboBox::from_id_salt("engine_profile")
                    .selected_text(self.engine.name())
                    .show_ui(ui, |ui| {
                        for &eng in EngineProfile::all() {
                            ui.selectable_value(&mut self.engine, eng, eng.name());
                        }
                    });
                ui.separator();
                if ui.button(format!("Theme: {}", self.theme.name())).clicked() {
                    self.theme = self.theme.toggled();
                }
                if ui.button("Reset layout").clicked() {
                    self.dock = default_layout();
                }
            });
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let busy = self
                    .active_batch
                    .as_ref()
                    .map(Task::what)
                    .or_else(|| self.active_inspect.as_ref().map(Task::what));
                if let Some(what) = busy {
                    ui.label(RichText::new(format!("Running: {what}...")).color(pal.status_warn));
                } else {
                    ui.label(RichText::new("Idle").color(pal.ink_muted));
                }
                ui.separator();
                ui.label(RichText::new(format!("Queue: {} items", self.queue.len())).color(pal.ink_muted));
            });
        });

        let style = theme::dock_style(ui.style().as_ref(), self.theme);
        let mut dock = std::mem::replace(&mut self.dock, DockState::new(Vec::new()));
        DockArea::new(&mut dock)
            .style(style)
            .show_inside(ui, &mut Viewer { app: self });
        self.dock = dock;
    }
}
