//! The ShortEnd window: Offload, Progress, History and Presets views over `shortend-core`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use shortend_core::preset::{self, Preset, PresetError};
use shortend_core::report::{human_bytes, human_duration};
use shortend_core::{Error, HashAlgorithm, Job, JobListing, JobState};

use crate::form::{DestRow, Form};
use crate::live::{Running, Task, lock};

/// How often a running job's view refreshes. Input still renders immediately.
const LIVE_REFRESH: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Offload,
    Progress,
    History,
    Presets,
}

/// Preflight state for the current form.
pub enum Check {
    None,
    Running(Task<Result<Job, Error>>, Form),
    /// Passed for exactly this form.
    Ready(Box<Job>, Form),
    /// Refused for exactly this form.
    Refused(Vec<String>, Form),
}

pub struct ShortEndApp {
    pub view: View,
    pub form: Form,
    pub check: Check,
    pub running: Option<Running>,
    presets: Vec<(String, PathBuf, Result<Preset, PresetError>)>,
    preset_choice: Option<String>,
    preset_message: Option<Result<String, String>>,
    history: Option<Task<Result<Vec<JobListing>, String>>>,
    pub jobs: Result<Vec<JobListing>, String>,
    history_loaded: bool,
    pub editor: Editor,
    confirm_close: bool,
    allow_close: bool,
    /// Duration of the last `ui` call, for the responsiveness test.
    pub last_frame: Duration,
}

/// The Presets view's TOML editor.
#[derive(Default)]
pub struct Editor {
    pub name: String,
    pub text: String,
    /// The text is valid as a preset (or why not); recomputed after each edit.
    pub status: Option<Result<(), String>>,
    pub message: Option<Result<String, String>>,
}

impl Default for ShortEndApp {
    fn default() -> Self {
        Self::new()
    }
}

impl ShortEndApp {
    pub fn new() -> Self {
        Self {
            view: View::Offload,
            form: Form::default(),
            check: Check::None,
            running: None,
            presets: preset::list(),
            preset_choice: None,
            preset_message: None,
            history: None,
            jobs: Ok(Vec::new()),
            history_loaded: false,
            editor: Editor::default(),
            confirm_close: false,
            allow_close: false,
            last_frame: Duration::ZERO,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.as_ref().is_some_and(Running::is_running)
    }

    /// One frame of the whole window.
    pub fn ui(&mut self, ctx: &egui::Context) {
        let t0 = Instant::now();
        self.poll(ctx);
        self.handle_close(ctx);
        self.handle_drops(ctx);

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("ShortEnd").strong());
                ui.add_space(16.0);
                for (v, name) in [
                    (View::Offload, "Offload"),
                    (View::Progress, "Progress"),
                    (View::History, "History"),
                    (View::Presets, "Presets"),
                ] {
                    let label = if v == View::Progress && self.is_running() {
                        "Progress (running)".to_owned()
                    } else {
                        name.to_owned()
                    };
                    if ui.selectable_label(self.view == v, label).clicked() {
                        self.view = v;
                        if v == View::History && !self.history_loaded {
                            self.refresh_history(ctx);
                        }
                    }
                }
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.view {
            View::Offload => self.offload_view(ui),
            View::Progress => self.progress_view(ui),
            View::History => self.history_view(ui),
            View::Presets => self.presets_view(ui),
        });

        if self.is_running() {
            ctx.request_repaint_after(LIVE_REFRESH);
        }
        self.last_frame = t0.elapsed();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        // A check result only stands for the form it was made for.
        if let Check::Ready(_, f) | Check::Refused(_, f) = &self.check
            && *f != self.form
        {
            self.check = Check::None;
        }
        if let Check::Running(task, form) = &self.check
            && let Some(result) = task.take()
        {
            let form = form.clone();
            self.check = match result {
                Ok(job) => Check::Ready(Box::new(job), form),
                Err(Error::Preflight(p)) => Check::Refused(p, form),
                Err(e) => Check::Refused(vec![e.to_string()], form),
            };
        }
        if let Some(task) = &self.history
            && let Some(jobs) = task.take()
        {
            self.jobs = jobs;
            self.history = None;
        }
        let _ = ctx;
    }

    /// Closing the window while a job runs asks first; confirming cancels the job, which
    /// stays resumable.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.is_running() && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        if !self.confirm_close {
            return;
        }
        egui::Modal::new(egui::Id::new("confirm-close")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("An offload is running");
            ui.label(
                "Closing ShortEnd cancels it. Files already verified are kept; the job can be \
                 resumed later from History.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Keep running").clicked() {
                    self.confirm_close = false;
                }
                if ui.button("Cancel job and quit").clicked() {
                    if let Some(r) = &self.running {
                        r.cancel();
                    }
                    self.allow_close = true;
                    self.confirm_close = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    /// A folder dropped on the window becomes the source.
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if let Some(dir) = dropped.into_iter().find(|p| p.is_dir()) {
            self.form.source = dir.display().to_string();
            self.view = View::Offload;
        }
    }

    // --- Offload -------------------------------------------------------------------------

    fn offload_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.preset_row(ui);
                ui.add_space(6.0);
                section(ui, "Source and destinations", |ui| self.paths_section(ui));
                section(ui, "Folder", |ui| self.folder_section(ui));
                section(ui, "Production", |ui| self.production_section(ui));
                section(ui, "Verification and reports", |ui| {
                    self.settings_section(ui)
                });
                ui.add_space(8.0);
                self.check_section(ui);
            });
    }

    fn preset_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Preset");
            let selected = self.preset_choice.clone().unwrap_or_else(|| "—".into());
            egui::ComboBox::from_id_salt("preset")
                .selected_text(selected)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for (name, _, loaded) in &self.presets {
                        let text = if loaded.is_ok() {
                            name.clone()
                        } else {
                            format!("{name} (invalid)")
                        };
                        ui.selectable_value(&mut self.preset_choice, Some(name.clone()), text);
                    }
                });
            let chosen = self
                .preset_choice
                .as_ref()
                .and_then(|c| self.presets.iter().find(|(n, ..)| n == c));
            if ui
                .add_enabled(chosen.is_some(), egui::Button::new("Apply preset"))
                .on_hover_text("Fill the form from the preset; the source stays")
                .clicked()
                && let Some((name, _, loaded)) = chosen
            {
                self.preset_message = Some(match loaded {
                    Ok(p) => {
                        self.form = self.form.apply_preset(p);
                        Ok(format!("applied preset {name}"))
                    }
                    Err(e) => Err(e.message.clone()),
                });
            }
            if ui.button("Reload").clicked() {
                self.presets = preset::list();
            }
            match &self.preset_message {
                Some(Ok(m)) => {
                    ui.weak(m);
                }
                Some(Err(m)) => {
                    ui.colored_label(ui.visuals().error_fg_color, m);
                }
                None => {}
            }
        });
    }

    fn paths_section(&mut self, ui: &mut egui::Ui) {
        labeled(ui, "Source", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.form.source)
                    .hint_text("card or folder (or drop it on the window)")
                    .desired_width(DEST_LABEL_W + PATH_W + ui.spacing().item_spacing.x),
            );
            if ui.button("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.form.source = p.display().to_string();
            }
        });
        let mut remove = None;
        let n = self.form.dests.len();
        for (i, d) in self.form.dests.iter_mut().enumerate() {
            labeled(ui, &format!("Destination {}", i + 1), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut d.label)
                        .hint_text("label")
                        .desired_width(DEST_LABEL_W),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut d.root)
                        .hint_text("drive or folder")
                        .desired_width(PATH_W),
                );
                if ui.button("Browse…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    if d.label.is_empty() {
                        d.label = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                    }
                    d.root = p.display().to_string();
                }
                if n > 1 && ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.form.dests.remove(i);
        }
        labeled(ui, "", |ui| {
            if ui.button("+ Add destination").clicked() {
                self.form.dests.push(DestRow::default());
            }
        });
    }

    fn folder_section(&mut self, ui: &mut egui::Ui) {
        labeled(ui, "Template", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.form.folder)
                    .hint_text("{show}/{date}_D{day}/{camroll}_{seq:3}   (empty: card name)")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(DEST_LABEL_W + PATH_W + ui.spacing().item_spacing.x),
            );
        });
        labeled(ui, "Creates", |ui| match self.form.folder_preview() {
            Ok(f) => {
                ui.monospace(f);
            }
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        });
        ui.weak("Tokens: {date[:fmt]} {time[:fmt]} {show} {episode} {day} {card} {camroll} {seq[:width]}");
    }

    fn production_section(&mut self, ui: &mut egui::Ui) {
        let f = &mut self.form;
        let pairs: [[(&str, &mut String); 2]; 3] = [
            [("Show", &mut f.show), ("Episode", &mut f.episode)],
            [("Shoot day", &mut f.day), ("Camera roll", &mut f.camroll)],
            [("Operator", &mut f.operator), ("Company", &mut f.company)],
        ];
        for [(a, va), (b, vb)] in pairs {
            labeled(ui, a, |ui| {
                ui.add(egui::TextEdit::singleline(va).desired_width(FIELD_W));
                ui.add_space(12.0);
                fixed_label(ui, b);
                ui.add(egui::TextEdit::singleline(vb).desired_width(FIELD_W));
            });
        }
        labeled(ui, "Notes", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut f.notes).desired_width(
                    2.0 * FIELD_W + LABEL_W + 12.0 + 2.0 * ui.spacing().item_spacing.x,
                ),
            );
        });
    }

    fn settings_section(&mut self, ui: &mut egui::Ui) {
        let f = &mut self.form;
        ui.horizontal(|ui| {
            ui.label("Checksum");
            egui::ComboBox::from_id_salt("primary")
                .selected_text(f.primary.to_string())
                .show_ui(ui, |ui| {
                    for a in HashAlgorithm::ALL {
                        ui.selectable_value(&mut f.primary, a, a.to_string());
                    }
                });
            ui.label("also compute");
            for a in HashAlgorithm::ALL {
                if a == f.primary {
                    continue;
                }
                let mut on = f.extra.contains(&a);
                if ui.checkbox(&mut on, a.to_string()).changed() {
                    if on {
                        f.extra.push(a);
                    } else {
                        f.extra.retain(|x| *x != a);
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Manifests");
            ui.checkbox(&mut f.mhl_v2, "ASC MHL v2");
            ui.checkbox(&mut f.mhl_v11, "MHL v1.1");
            ui.separator();
            ui.label("Reports");
            ui.checkbox(&mut f.csv, "CSV");
            ui.checkbox(&mut f.txt, "TXT");
            ui.checkbox(&mut f.pdf, "PDF");
        });
        if f.pdf {
            ui.horizontal(|ui| {
                ui.label("PDF logo");
                ui.add(
                    egui::TextEdit::singleline(&mut f.logo)
                        .hint_text("PNG or JPEG (optional)")
                        .desired_width(340.0),
                );
                if ui.button("Browse…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("Image", &["png", "jpg", "jpeg"])
                        .pick_file()
                {
                    f.logo = p.display().to_string();
                }
            });
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut f.reread, "Re-read the source after copying");
            ui.separator();
            ui.label("Files at once");
            ui.add(egui::DragValue::new(&mut f.parallel).range(1..=16));
            ui.separator();
            ui.label("Drop a drive after");
            ui.add(
                egui::DragValue::new(&mut f.stall_timeout)
                    .range(0..=3600)
                    .suffix(" s"),
            )
            .on_hover_text("Seconds without progress before a destination is dropped (0: never)");
        });
    }

    fn check_section(&mut self, ui: &mut egui::Ui) {
        let running = self.is_running();
        ui.horizontal(|ui| {
            let checking = matches!(self.check, Check::Running(..));
            if ui
                .add_enabled(
                    !checking,
                    egui::Button::new("Check").min_size(egui::vec2(90.0, 28.0)),
                )
                .on_hover_text("Scan the source and check every destination; nothing is written")
                .clicked()
            {
                self.start_check(ui.ctx());
            }
            let ready = matches!(&self.check, Check::Ready(_, f) if *f == self.form);
            let start = ui
                .add_enabled(
                    ready && !running,
                    egui::Button::new(RichText::new("Start offload").strong())
                        .min_size(egui::vec2(130.0, 28.0)),
                )
                .on_disabled_hover_text(if running {
                    "Another offload is running"
                } else {
                    "Check the job first"
                });
            if start.clicked()
                && let Check::Ready(job, _) = std::mem::replace(&mut self.check, Check::None)
            {
                self.running = Some(Running::start(ui.ctx(), *job, false));
                self.view = View::Progress;
            }
            if checking {
                ui.spinner();
                ui.label("Checking…");
            }
        });
        ui.add_space(6.0);
        match &self.check {
            Check::None | Check::Running(..) => {}
            Check::Refused(problems, _) => {
                let red = ui.visuals().error_fg_color;
                ui.colored_label(
                    red,
                    RichText::new("Refused — fix these and check again:").strong(),
                );
                for p in problems {
                    ui.colored_label(red, format!("-  {p}"));
                }
            }
            Check::Ready(job, _) => plan_summary(ui, job),
        }
    }

    fn start_check(&mut self, ctx: &egui::Context) {
        match self.form.to_spec() {
            Ok(spec) => {
                let task = Task::spawn(ctx, move || shortend_core::plan(spec));
                self.check = Check::Running(task, self.form.clone());
            }
            Err(e) => self.check = Check::Refused(vec![e], self.form.clone()),
        }
    }

    // --- Progress ------------------------------------------------------------------------

    fn progress_view(&mut self, ui: &mut egui::Ui) {
        let Some(running) = &self.running else {
            ui.add_space(20.0);
            ui.label("No offload yet. Set one up in Offload, or resume one from History.");
            return;
        };
        let mut new_offload = false;
        let mut cancel = false;
        {
            let l = lock(&running.live);
            let elapsed = l.finished_at.unwrap_or_else(Instant::now) - l.started;
            ui.horizontal(|ui| {
                let (text, color) = match &l.result {
                    None if l.cancelling => ("Cancelling…".to_owned(), ui.visuals().warn_fg_color),
                    None => ("Copying".to_owned(), ui.visuals().text_color()),
                    Some(Ok(r)) => match r.state {
                        JobState::Completed => ("PASS".to_owned(), Color32::from_rgb(40, 160, 70)),
                        JobState::CompletedWithFailures => {
                            ("FAIL".to_owned(), ui.visuals().error_fg_color)
                        }
                        JobState::Cancelled => ("Cancelled".to_owned(), ui.visuals().warn_fg_color),
                        JobState::Aborted => (
                            "Stopped: the source became unavailable".to_owned(),
                            ui.visuals().error_fg_color,
                        ),
                    },
                    Some(Err(e)) => (format!("Error: {e}"), ui.visuals().error_fg_color),
                };
                ui.heading(RichText::new(text).color(color).strong());
                ui.add_space(12.0);
                ui.label(format!(
                    "{}{}  ·  {}",
                    if l.resumed { "resumed · " } else { "" },
                    l.job.folder,
                    human_duration(elapsed)
                ));
            });
            ui.add_space(6.0);

            let total = l.total_bytes.max(1);
            let frac = l.source_bytes as f32 / total as f32;
            let eta = if l.running() && l.source_rate.per_sec() > 0.0 {
                let left = l.total_bytes.saturating_sub(l.source_bytes) as f64;
                format!(
                    "  ·  {} left",
                    human_duration(Duration::from_secs_f64(left / l.source_rate.per_sec()))
                )
            } else {
                String::new()
            };
            ui.label(format!(
                "Source  {} of {}  ·  {}/s{eta}",
                human_bytes(l.source_bytes),
                human_bytes(l.total_bytes),
                human_bytes(l.source_rate.per_sec() as u64)
            ));
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            ui.add_space(6.0);

            egui::Grid::new("dests")
                .num_columns(3)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    for (i, d) in l.job.destinations.iter().enumerate() {
                        ui.label(RichText::new(&d.label).strong());
                        let b = l.dest_bytes.get(i).copied().unwrap_or(0);
                        let bar = egui::ProgressBar::new(b as f32 / total as f32)
                            .desired_width(360.0)
                            .text(format!(
                                "{}  ·  {}/s",
                                human_bytes(b),
                                human_bytes(l.dest_rates[i].per_sec() as u64)
                            ));
                        ui.add(bar);
                        match &l.dest_failed[i] {
                            Some(f) => {
                                ui.colored_label(
                                    ui.visuals().error_fg_color,
                                    format!("FAILED: {f}"),
                                );
                            }
                            None => {
                                ui.weak(format!("{} ({})", d.target.display(), d.volume.fs_type));
                            }
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(6.0);
            ui.label(format!(
                "Files  {} of {} done{}",
                l.files_done,
                l.job.files.len(),
                if l.already_verified > 0 {
                    format!("  ({} already verified earlier)", l.already_verified)
                } else {
                    String::new()
                }
            ));
            for c in &l.current {
                ui.monospace(format!("→ {c}"));
            }

            if let Some(Ok(rep)) = &l.result {
                ui.add_space(6.0);
                for issue in &rep.manifest_issues {
                    ui.colored_label(ui.visuals().error_fg_color, format!("Manifest: {issue}"));
                }
                if !rep.manifest_omissions.is_empty() {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!(
                            "Not in the MHL v1.1 manifest ({} zero-byte files): {}",
                            rep.manifest_omissions.len(),
                            rep.manifest_omissions.join(", ")
                        ),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    for d in &rep.job.destinations {
                        let reports = d.target.join(shortend_core::engine::preflight::REPORT_DIR);
                        if reports.is_dir()
                            && ui.button(format!("Open {} reports", d.label)).clicked()
                        {
                            open_path(&reports);
                        }
                    }
                });
                if matches!(rep.state, JobState::Cancelled | JobState::Aborted) {
                    ui.label(format!("Resume it from History (job {}).", rep.job.id));
                }
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if l.running() {
                    if ui
                        .add_enabled(!l.cancelling, egui::Button::new("Cancel"))
                        .on_hover_text(
                            "Stop now; files already verified are kept and the job can be resumed",
                        )
                        .clicked()
                    {
                        cancel = true;
                    }
                } else if ui.button("New offload").clicked() {
                    new_offload = true;
                }
            });

            list_block(ui, "Failures", &l.failures, ui.visuals().error_fg_color);
            list_block(ui, "Warnings", &l.warnings, ui.visuals().warn_fg_color);
        }
        if cancel {
            running.cancel();
        }
        if new_offload {
            self.view = View::Offload;
            self.check = Check::None;
        }
    }

    // --- History -------------------------------------------------------------------------

    fn refresh_history(&mut self, ctx: &egui::Context) {
        self.history_loaded = true;
        self.history = Some(Task::spawn(ctx, || {
            shortend_core::list_jobs().map_err(|e| e.to_string())
        }));
    }

    fn history_view(&mut self, ui: &mut egui::Ui) {
        if !self.history_loaded {
            self.refresh_history(ui.ctx());
        }
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.refresh_history(ui.ctx());
            }
            if self.history.is_some() {
                ui.spinner();
            }
        });
        ui.add_space(6.0);
        let jobs = match &self.jobs {
            Ok(j) => j.clone(),
            Err(e) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
                return;
            }
        };
        if jobs.is_empty() {
            ui.label("No jobs yet.");
            return;
        }
        let busy = self.is_running();
        let mut resume = None;
        let row_h = ui.spacing().interact_size.y + 4.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, jobs.len(), |ui, range| {
                for j in jobs.iter().rev().skip(range.start).take(range.len()) {
                    ui.horizontal(|ui| {
                        let when = j
                            .created
                            .to_zoned(jiff::tz::TimeZone::system())
                            .strftime("%Y-%m-%d %H:%M")
                            .to_string();
                        ui.monospace(when);
                        let (state, color) = match j.state {
                            Some(JobState::Completed) => {
                                ("completed", Color32::from_rgb(40, 160, 70))
                            }
                            Some(JobState::CompletedWithFailures) => {
                                ("failures", ui.visuals().error_fg_color)
                            }
                            Some(JobState::Cancelled) => ("cancelled", ui.visuals().warn_fg_color),
                            Some(JobState::Aborted) => ("stopped", ui.visuals().warn_fg_color),
                            None => ("interrupted", ui.visuals().warn_fg_color),
                        };
                        ui.add_sized(
                            [86.0, row_h - 4.0],
                            egui::Label::new(RichText::new(state).color(color)),
                        );
                        ui.label(format!(
                            "{}  to  {}  ({} destination{})",
                            j.source.display(),
                            j.folder,
                            j.destinations,
                            if j.destinations == 1 { "" } else { "s" }
                        ));
                        let resumable = !matches!(j.state, Some(JobState::Completed));
                        if resumable
                            && ui
                                .add_enabled(!busy, egui::Button::new("Resume"))
                                .on_hover_text("Re-check what was copied and copy the rest")
                                .clicked()
                        {
                            resume = Some(j.id.to_string());
                        }
                        if ui.small_button("Reports").clicked()
                            && let Ok(job) = shortend_core::load_job(&j.id.to_string())
                            && let Some(d) = job.destinations.first()
                        {
                            open_path(&d.target.join(shortend_core::engine::preflight::REPORT_DIR));
                        }
                    });
                }
            });
        if let Some(id) = resume {
            match shortend_core::load_job(&id) {
                Ok(job) => {
                    self.running = Some(Running::start(ui.ctx(), job, true));
                    self.view = View::Progress;
                    self.history_loaded = false;
                }
                Err(e) => self.jobs = Err(e.to_string()),
            }
        }
    }

    // --- Presets -------------------------------------------------------------------------

    fn presets_view(&mut self, ui: &mut egui::Ui) {
        egui::SidePanel::left("preset-list")
            .resizable(false)
            .exact_width(220.0)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Presets");
                    if ui.small_button("Reload").clicked() {
                        self.presets = preset::list();
                    }
                });
                ui.weak(preset::presets_dir().display().to_string());
                ui.add_space(6.0);
                for (name, path, loaded) in &self.presets {
                    let text = match loaded {
                        Ok(_) => RichText::new(name),
                        Err(_) => RichText::new(format!("{name} (invalid)"))
                            .color(ui.visuals().error_fg_color),
                    };
                    if ui
                        .selectable_label(self.editor.name == *name, text)
                        .clicked()
                    {
                        self.editor.name = name.clone();
                        self.editor.text = std::fs::read_to_string(path).unwrap_or_default();
                        self.editor.status = None;
                        self.editor.message = None;
                    }
                }
                ui.add_space(8.0);
                if ui.button("New from Offload form").clicked() {
                    match self.form.to_preset() {
                        Ok(p) => {
                            self.editor.text = p.to_toml();
                            self.editor.name = "new-preset".into();
                            self.editor.message = None;
                        }
                        Err(e) => self.editor.message = Some(Err(e)),
                    }
                    self.editor.status = None;
                }
            });

        let e = &mut self.editor;
        if e.text.is_empty() && e.name.is_empty() {
            ui.add_space(20.0);
            ui.label("Select a preset on the left, or create one from the Offload form.");
            ui.weak("Presets are TOML files you can share; flags and form fields override them.");
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut e.name).desired_width(200.0));
        });
        let status = e
            .status
            .get_or_insert_with(|| {
                Preset::from_toml(&e.text, None)
                    .map(|_| ())
                    .map_err(|x| x.message)
            })
            .clone();
        let edited = egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 70.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut e.text)
                        .code_editor()
                        .desired_rows(22)
                        .desired_width(f32::INFINITY),
                )
                .changed()
            })
            .inner;
        if edited {
            e.status = None;
            e.message = None;
        }
        match &status {
            Ok(()) => {
                ui.colored_label(Color32::from_rgb(40, 160, 70), "Valid preset");
            }
            Err(m) => {
                ui.colored_label(ui.visuals().error_fg_color, m);
            }
        }
        let name_ok = !e.name.trim().is_empty() && !e.name.contains(['/', '\\']);
        let mut save = false;
        let mut use_it = false;
        ui.horizontal(|ui| {
            save = ui
                .add_enabled(status.is_ok() && name_ok, egui::Button::new("Save"))
                .clicked();
            use_it = ui
                .add_enabled(status.is_ok(), egui::Button::new("Use in Offload"))
                .clicked();
            match &e.message {
                Some(Ok(m)) => {
                    ui.weak(m);
                }
                Some(Err(m)) => {
                    ui.colored_label(ui.visuals().error_fg_color, m);
                }
                None => {}
            }
        });
        if save {
            let path = preset::presets_dir().join(format!("{}.toml", e.name.trim()));
            let result = Preset::from_toml(&e.text, path.parent()).and_then(|_| {
                std::fs::create_dir_all(preset::presets_dir())
                    .and_then(|()| std::fs::write(&path, &e.text))
                    .map_err(|x| PresetError {
                        path: Some(path.clone()),
                        message: x.to_string(),
                    })
            });
            e.message = Some(
                result
                    .map(|()| format!("saved {}", path.display()))
                    .map_err(|x| x.to_string()),
            );
            self.presets = preset::list();
        }
        if use_it && let Ok(p) = Preset::from_toml(&e.text, Some(&preset::presets_dir())) {
            self.form = self.form.apply_preset(&p);
            self.preset_choice = Some(e.name.clone());
            self.view = View::Offload;
        }
    }
}

impl eframe::App for ShortEndApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).strong().size(15.0));
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        body(ui);
    });
}

const LABEL_W: f32 = 96.0;
const FIELD_W: f32 = 230.0;
const DEST_LABEL_W: f32 = 110.0;
const PATH_W: f32 = 380.0;

fn fixed_label(ui: &mut egui::Ui, text: &str) {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(LABEL_W, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(LABEL_W);
            ui.label(text);
        },
    );
}

/// A form row: a fixed-width label, then the row's widgets.
fn labeled<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        fixed_label(ui, label);
        add(ui)
    })
    .inner
}

fn plan_summary(ui: &mut egui::Ui, job: &Job) {
    let green = Color32::from_rgb(40, 160, 70);
    ui.colored_label(green, RichText::new("Ready to offload").strong());
    ui.label(format!(
        "{} files, {} from {}",
        job.files.len(),
        human_bytes(job.total_bytes()),
        job.source.root.display()
    ));
    ui.label(format!("Checksums: {}", job.settings.hashes));
    for d in &job.destinations {
        ui.label(format!(
            "{}:  {}  ({})",
            d.label,
            d.target.display(),
            d.volume.fs_type
        ));
    }
    if job.seq.is_some() {
        ui.weak("The folder number is claimed on every destination when the copy starts.");
    }
    for w in &job.warnings {
        ui.colored_label(ui.visuals().warn_fg_color, format!("warning: {w}"));
    }
}

/// A collapsible, virtualised list (cheap with tens of thousands of rows).
fn list_block(ui: &mut egui::Ui, title: &str, rows: &[String], color: Color32) {
    if rows.is_empty() {
        return;
    }
    ui.add_space(6.0);
    egui::CollapsingHeader::new(RichText::new(format!("{title} ({})", rows.len())).color(color))
        .default_open(true)
        .show(ui, |ui| {
            let h = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::vertical()
                .id_salt(title)
                .max_height(220.0)
                .auto_shrink([false, true])
                .show_rows(ui, h, rows.len(), |ui, range| {
                    for r in &rows[range] {
                        ui.monospace(r);
                    }
                });
        });
}

/// Opens a folder in the system file manager.
fn open_path(path: &Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(path).spawn();
}
