use crate::track_drag::{self, TrackDrag};
use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, Vec2};
use loudtrace::audio::{self, AlbumMeasurement, Measurement, TrackAnalysis};
use loudtrace::playback::{Playback, PlaybackState};
use loudtrace::policy::normal_gain;
use loudtrace::project::{
    AnalysisMode, FileStamp, Library, Project, ProjectStore, Track, file_stamp,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::ui_style::{
    ACCENT, ACCENT_EDGE, ACCENT_HOVER, ACCENT_INK, ACCENT_PRESSED, ACCENT_TINT, BG, DISABLED,
    EDGE_HOVER, FAINT, HAIRLINE, INK, Icon, LINE, MUTED, RAIL, RAISED, SELECTED, SIDEBAR, T_BODY,
    T_BODY_MEDIUM, T_BODY_STRONG, T_BRAND, T_CARD, T_LABEL, T_NUM, T_NUM_SMALL, T_SMALL,
    T_SMALL_MEDIUM, T_TITLE, Type, WARNING, WARNING_EDGE, WARNING_TINT, configure_style,
    icon_button, icon_button_reveal, layout_rich, layout_text, paint_icon, paint_logo,
    paint_slider_handle, play_button, style_slider,
};

// One track's analysis. It describes the track only while the track still
// points at `path` and the file still matches `stamp`.
#[derive(Debug)]
struct TrackResult {
    path: PathBuf,
    stamp: Option<FileStamp>,
    analysis: Result<TrackAnalysis, String>,
}

// Tracks are analyzed independently and keyed by ID, so editing, reordering, or
// changing one track never discards the others. The album is derived from them.
#[derive(Debug, Default)]
struct Report {
    tracks: HashMap<Uuid, TrackResult>,
    // None while any track still needs analysis.
    album: Option<Result<AlbumMeasurement, String>>,
}

impl Report {
    fn result(&self, track: &Track) -> Option<&Result<TrackAnalysis, String>> {
        self.tracks
            .get(&track.id)
            .filter(|result| result.path == track.path)
            .map(|result| &result.analysis)
    }

    fn measurement(&self, track: &Track) -> Option<&Measurement> {
        let analysis = self.result(track)?.as_ref().ok()?;
        Some(&analysis.measurement)
    }

    // Drops results for removed or relinked tracks, then pools the album again.
    fn sync(&mut self, project: &Project) {
        self.tracks.retain(|id, result| {
            project
                .tracks
                .iter()
                .any(|track| track.id == *id && track.path == result.path)
        });
        self.album = self.pooled_album(project);
    }

    fn pooled_album(&self, project: &Project) -> Option<Result<AlbumMeasurement, String>> {
        if project.tracks.is_empty() {
            return None;
        }
        let mut analyses = Vec::with_capacity(project.tracks.len());
        let mut failed = false;
        for track in &project.tracks {
            match self.result(track)? {
                Ok(analysis) => analyses.push(analysis),
                Err(_) => failed = true,
            }
        }
        Some(if failed {
            Err("Every track must be analyzed successfully before calculating the album.".into())
        } else {
            audio::album_measurement(analyses)
        })
    }
}

enum WorkerEvent {
    Track(Uuid, Box<TrackResult>),
    Finished,
}

struct Worker {
    project_id: Uuid,
    rx: mpsc::Receiver<WorkerEvent>,
    cancelled: Arc<AtomicBool>,
    completed: usize,
    total: usize,
}

struct Audition {
    session_id: Uuid,
    project_id: Uuid,
    track_id: Uuid,
    finished: bool,
}

enum Dialog {
    Create(String),
    Rename(Uuid, String),
    Delete(Uuid),
}

enum Action {
    Open(Uuid),
    Back,
    AddFiles,
    Replace(Uuid),
    Remove(Uuid),
    Move(Uuid, isize),
    Reorder {
        project_id: Uuid,
        track_id: Uuid,
        insert_before: Option<Uuid>,
    },
    Mode(AnalysisMode),
    Analyze,
    Cancel,
    Rename,
    Delete,
    Create,
    PlayTrack(Uuid),
    TogglePlayback,
    StopPlayback,
    Skip(isize),
    Seek {
        session_id: Uuid,
        position: Duration,
    },
    Volume(f32),
    Mute,
}

pub struct LoudTraceApp {
    library: Library,
    store: ProjectStore,
    _library_lock: Option<std::fs::File>,
    read_only: bool,
    selected: Option<Uuid>,
    reports: HashMap<Uuid, Report>,
    worker: Option<Worker>,
    dialog: Option<Dialog>,
    notice: Option<String>,
    last_file_check: Instant,
    playback: Playback,
    audition: Option<Audition>,
    monitor_volume: f32,
    muted: bool,
    seek_preview: Option<f64>,
}

impl LoudTraceApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, String> {
        configure_style(&cc.egui_ctx)?;
        let store = ProjectStore::new(ProjectStore::default_path());
        Ok(Self::load(store))
    }

    fn load(store: ProjectStore) -> Self {
        let mut notices = Vec::new();
        let library_lock = match store.acquire_lock() {
            Ok(lock) => Some(lock),
            Err(error) => {
                notices.push(format!(
                    "Projects are read-only. {error} Close other LoudTrace windows or restore access to the data directory, then reopen this window."
                ));
                None
            }
        };
        let library = match store.load() {
            Ok(library) => library,
            Err(error) => {
                notices.push(format!(
                    "Could not load your projects. Existing data has been preserved. {error}"
                ));
                Library::default()
            }
        };
        let read_only = !notices.is_empty();
        let notice = read_only.then(|| notices.join(" "));
        Self {
            library,
            store,
            _library_lock: library_lock,
            read_only,
            selected: None,
            reports: HashMap::new(),
            worker: None,
            dialog: None,
            notice,
            last_file_check: Instant::now(),
            playback: Playback::new(),
            audition: None,
            monitor_volume: 0.25,
            muted: false,
            seek_preview: None,
        }
    }

    fn project(&self) -> Option<&Project> {
        let id = self.selected?;
        self.library
            .projects
            .iter()
            .find(|project| project.id == id)
    }

    fn editable(&self) -> bool {
        !self.read_only && self.worker.is_none()
    }

    fn commit(&mut self, edit: impl FnOnce(&mut Library)) -> bool {
        if self.read_only {
            return false;
        }
        let mut next = self.library.clone();
        edit(&mut next);
        match self.store.save(&next) {
            Ok(()) => {
                self.library = next;
                true
            }
            Err(error) => {
                self.notice = Some(format!("Changes were not saved: {error}"));
                false
            }
        }
    }

    fn edit_project(&mut self, edit: impl FnOnce(&mut Project)) {
        if !self.editable() {
            return;
        }
        if let Some(id) = self.selected {
            let committed = self.commit(|library| {
                if let Some(project) = library.projects.iter_mut().find(|p| p.id == id) {
                    edit(project);
                }
            });
            if committed {
                // Untouched tracks keep their results; the album gain may change.
                self.sync_report(id);
                self.sync_playback_gain();
            }
        }
    }

    // Re-pairs a project's results with its current tracks. A report with no
    // results left is removed, so it again reads as "Ready to analyze".
    fn sync_report(&mut self, project_id: Uuid) {
        let project = self.library.projects.iter().find(|p| p.id == project_id);
        let Some(report) = self.reports.get_mut(&project_id) else {
            return;
        };
        if let Some(project) = project {
            report.sync(project);
        }
        if project.is_none() || report.tracks.is_empty() {
            self.reports.remove(&project_id);
        }
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.edit_project(|project| {
            for path in paths {
                // Repeated imports should not accidentally count one file twice in an album.
                if !project.tracks.iter().any(|track| track.path == path) {
                    project.tracks.push(Track::new(path));
                }
            }
        });
    }

    fn move_track(&mut self, track_id: Uuid, delta: isize) {
        let Some(project) = self.project() else {
            return;
        };
        let Some(index) = project.tracks.iter().position(|track| track.id == track_id) else {
            return;
        };
        let Some(other) = index
            .checked_add_signed(delta)
            .filter(|i| *i < project.tracks.len() && *i != index)
        else {
            return;
        };
        let insert_before = project
            .tracks
            .get(if other < index { other } else { other + 1 })
            .map(|track| track.id);
        self.reorder_track(project.id, track_id, insert_before);
    }

    fn reorder_track(&mut self, project_id: Uuid, track_id: Uuid, insert_before: Option<Uuid>) {
        if !self.editable() || self.selected != Some(project_id) {
            return;
        }
        let Some(project) = self.project() else {
            return;
        };
        let Some(index) = project.tracks.iter().position(|track| track.id == track_id) else {
            return;
        };
        let target = match insert_before {
            Some(target_id) => {
                let Some(target) = project
                    .tracks
                    .iter()
                    .position(|track| track.id == target_id)
                else {
                    return;
                };
                target
            }
            None => project.tracks.len(),
        };
        // Removing an earlier track shifts the insertion boundary one position left.
        let destination = target - usize::from(index < target);
        if destination == index {
            return;
        }
        // Results and playback follow track IDs, and pooled album loudness is
        // independent of order, so reordering leaves every result in place.
        self.commit(|library| {
            if let Some(project) = library.projects.iter_mut().find(|p| p.id == project_id) {
                let track = project.tracks.remove(index);
                project.tracks.insert(destination, track);
            }
        });
    }

    fn start_analysis(&mut self, ctx: &egui::Context) {
        if self.worker.is_some() {
            return;
        }
        let Some(project) = self.project() else {
            return;
        };
        let id = project.id;
        // Measure only tracks without a current result; failed tracks are
        // retried. When every track is current, measure all of them again.
        let mut queue: Vec<_> = unmeasured(self.reports.get(&id), project)
            .map(|t| (t.id, t.path.clone()))
            .collect();
        if queue.is_empty() {
            queue = project
                .tracks
                .iter()
                .map(|t| (t.id, t.path.clone()))
                .collect();
        }
        if queue.is_empty() {
            return;
        }
        let total = queue.len();
        if !self.read_only {
            self.notice = None;
        }
        // Queued tracks read as pending until their new result arrives.
        if let Some(report) = self.reports.get_mut(&id) {
            for (track_id, _) in &queue {
                report.tracks.remove(track_id);
            }
        }
        self.sync_report(id);
        self.sync_playback_gain();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for (track_id, path) in queue {
                // A result must describe one stable version of its file.
                let before = file_stamp(&path).ok();
                let analysis = audio::analyze_track(&path, &flag);
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let stamp = file_stamp(&path).ok();
                let analysis = if stamp == before {
                    analysis
                } else {
                    Err("File changed during analysis. Analyze again.".into())
                };
                let result = TrackResult {
                    path,
                    stamp,
                    analysis,
                };
                let _ = tx.send(WorkerEvent::Track(track_id, Box::new(result)));
                ctx.request_repaint();
            }
            let _ = tx.send(WorkerEvent::Finished);
            ctx.request_repaint();
        });
        self.worker = Some(Worker {
            project_id: id,
            rx,
            cancelled,
            completed: 0,
            total,
        });
    }

    fn poll_worker(&mut self) {
        let Some(worker) = &mut self.worker else {
            return;
        };
        let project_id = worker.project_id;
        let mut results = Vec::new();
        let mut finished = false;
        let mut disconnected = false;
        loop {
            match worker.rx.try_recv() {
                Ok(WorkerEvent::Track(track_id, result)) => {
                    worker.completed += 1;
                    results.push((track_id, *result));
                }
                Ok(WorkerEvent::Finished) => {
                    finished = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let cancelled = worker.cancelled.load(Ordering::Relaxed);
        // Each track is complete on its own, so results are kept as they
        // arrive, including those finished before a cancellation.
        if !results.is_empty() {
            self.reports
                .entry(project_id)
                .or_default()
                .tracks
                .extend(results);
            self.sync_report(project_id);
        }
        if finished {
            self.worker = None;
            if cancelled {
                self.notice =
                    Some("Analysis cancelled. Tracks that finished keep their results.".into());
            }
        } else if disconnected {
            self.worker = None;
            self.notice = Some("Analysis stopped unexpectedly. Please try again.".into());
        }
    }

    fn check_changed_files(&mut self) {
        if self.last_file_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_file_check = Instant::now();
        let ids: Vec<_> = self.reports.keys().copied().collect();
        for id in ids {
            if self.revalidate(id) && self.selected == Some(id) {
                self.notice = Some("A source file changed or became unavailable. Analyze again to update the results.".into());
            }
        }
    }

    // Drops results whose source changed since it was analyzed; other tracks
    // keep theirs. Returns whether any result was dropped.
    fn revalidate(&mut self, project_id: Uuid) -> bool {
        let Some(report) = self.reports.get_mut(&project_id) else {
            return false;
        };
        let count = report.tracks.len();
        report
            .tracks
            .retain(|_, result| file_stamp(&result.path).ok() == result.stamp);
        if report.tracks.len() == count {
            return false;
        }
        self.sync_report(project_id);
        self.sync_playback_gain();
        true
    }

    fn stop_playback(&mut self) {
        self.playback.stop();
        self.audition = None;
        self.seek_preview = None;
    }

    fn start_playback(&mut self, track_id: Uuid) {
        let Some(project_id) = self.selected else {
            return;
        };
        // Check the sources now rather than waiting for the periodic check.
        let changed = self.revalidate(project_id);
        if changed {
            self.notice = Some(
                "A source file changed or became unavailable. Analyze again to update the results."
                    .into(),
            );
        }
        let Some(project) = self.project() else {
            return;
        };
        let Some(track) = project.tracks.iter().find(|t| t.id == track_id) else {
            return;
        };
        let report = self.reports.get(&project_id);
        let Some(gain_db) = report.and_then(|r| audition_gain(r, project.mode, track)) else {
            let pending = report.is_none_or(|r| {
                r.result(track).is_none()
                    || (project.mode == AnalysisMode::Album && r.album.is_none())
            });
            self.stop_playback();
            self.notice = Some(if changed {
                "A source file changed. Analyze again before listening.".into()
            } else if pending {
                "Analyze the files before listening to the normalized result.".into()
            } else {
                "This track has no available normalization gain in the selected mode. Check the analysis result.".into()
            });
            return;
        };
        let Some(stamp) = report
            .and_then(|r| r.tracks.get(&track_id))
            .and_then(|r| r.stamp.clone())
        else {
            return;
        };
        let path = track.path.clone();
        self.playback
            .set_volume(if self.muted { 0.0 } else { self.monitor_volume });
        match self.playback.load(path, stamp, gain_db) {
            Ok(()) => {
                self.seek_preview = None;
                self.audition = Some(Audition {
                    session_id: Uuid::new_v4(),
                    project_id,
                    track_id,
                    finished: false,
                });
                // Keep the warning about another track's changed source.
                if !self.read_only && !changed {
                    self.notice = None;
                }
            }
            Err(error) => {
                self.stop_playback();
                self.notice = Some(format!("Cannot play this track: {error}"));
            }
        }
    }

    fn skip_playback(&mut self, delta: isize) {
        let Some(project) = self.project() else {
            return;
        };
        let current = self
            .audition
            .as_ref()
            .and_then(|a| project.tracks.iter().position(|t| t.id == a.track_id));
        let index = current
            .and_then(|i| i.checked_add_signed(delta))
            .unwrap_or(0);
        if let Some(track) = project.tracks.get(index) {
            self.start_playback(track.id);
        }
    }

    fn toggle_playback(&mut self) {
        match self.playback.state() {
            PlaybackState::Playing => self.playback.pause(),
            PlaybackState::Paused => self.playback.resume(),
            PlaybackState::Loading => self.stop_playback(),
            PlaybackState::Stopped | PlaybackState::Finished => {
                let track_id = self
                    .audition
                    .as_ref()
                    .map(|a| a.track_id)
                    .or_else(|| self.project()?.tracks.first().map(|t| t.id));
                if let Some(track_id) = track_id {
                    self.start_playback(track_id);
                }
            }
        }
    }

    // Re-applies the audition's gain from current results, or stops playback
    // when none remains. Returns whether it stopped for lack of a gain.
    fn sync_playback_gain(&mut self) -> bool {
        let Some(audition) = &self.audition else {
            return false;
        };
        let next_gain = self.project().and_then(|project| {
            let track = project.tracks.iter().find(|t| t.id == audition.track_id)?;
            audition_gain(self.reports.get(&project.id)?, project.mode, track)
        });
        let Some(gain_db) = next_gain else {
            self.stop_playback();
            return true;
        };
        if let Err(error) = self.playback.set_gain_db(gain_db) {
            self.stop_playback();
            self.notice = Some(error);
        }
        false
    }

    fn poll_playback(&mut self) {
        self.playback.poll();
        if let Some(error) = self.playback.take_error() {
            self.stop_playback();
            self.notice = Some(format!("Playback stopped: {error}"));
            return;
        }
        if self.playback.state() == PlaybackState::Finished
            && self.audition.as_ref().is_some_and(|a| !a.finished)
        {
            if let Some(audition) = &mut self.audition {
                audition.finished = true;
            }
            // Preserve the project order and stop on an unavailable result instead of skipping it.
            let next = self.project().and_then(|project| {
                let current = self.audition.as_ref()?.track_id;
                let index = project.tracks.iter().position(|t| t.id == current)?;
                project.tracks.get(index + 1).map(|t| t.id)
            });
            if let Some(track_id) = next {
                self.start_playback(track_id);
            }
        }
    }

    fn sidebar(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        let full = ui.max_rect();
        let width = ui.available_width();
        let (brand, _) = ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::hover());
        paint_logo(
            ui.painter(),
            egui::Rect::from_center_size(
                egui::pos2(brand.left() + 21.0, brand.top() + 15.0),
                Vec2::splat(22.0),
            ),
            INK,
            ACCENT,
        );
        paint_text(
            ui,
            egui::Rect::from_min_max(
                egui::pos2(brand.left() + 41.0, brand.top()),
                egui::pos2(brand.right(), brand.top() + 30.0),
            ),
            "LoudTrace",
            T_BRAND,
            INK,
            Align::Min,
        );

        section_label(ui, "LIBRARY");
        let all = nav_item(
            ui,
            Icon::Grid,
            "All projects",
            Some(self.library.projects.len()),
            self.selected.is_none(),
        );
        if all.clicked() && self.selected.is_some() {
            *action = Some(Action::Back);
        }

        ui.add_space(14.0);
        let (row, _) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::hover());
        paint_text(
            ui,
            egui::Rect::from_min_max(row.min + Vec2::new(10.0, 0.0), row.max),
            "PROJECTS",
            T_LABEL,
            FAINT,
            Align::Min,
        );
        if icon_at(
            ui,
            "sidebar_new_project",
            square_at(row.right() - 26.0, row.center().y, 24.0),
            Icon::Plus,
            "New project",
            self.editable(),
            false,
        )
        .clicked()
        {
            *action = Some(Action::Create);
        }
        ui.add_space(4.0);

        let footer_height = 30.0;
        let list_height = (ui.available_height() - footer_height - 10.0).max(0.0);
        egui::ScrollArea::vertical()
            .id_salt("sidebar_projects")
            .max_height(list_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if self.library.projects.is_empty() {
                    let (rect, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
                    paint_text(
                        ui,
                        rect.shrink2(Vec2::new(10.0, 0.0)),
                        "No projects yet",
                        T_SMALL,
                        FAINT,
                        Align::Min,
                    );
                }
                for project in &self.library.projects {
                    let selected = self.selected == Some(project.id);
                    let response = ui
                        .push_id(project.id, |ui| {
                            nav_item(
                                ui,
                                Icon::Bars,
                                &project.name,
                                Some(project.tracks.len()),
                                selected,
                            )
                        })
                        .inner;
                    if response.clicked() && !selected {
                        *action = Some(Action::Open(project.id));
                    }
                }
            });

        let footer_top = full.bottom() - footer_height;
        ui.painter().line_segment(
            [
                egui::pos2(full.left(), footer_top),
                egui::pos2(full.right(), footer_top),
            ],
            Stroke::new(1.0, LINE),
        );
        let footer = egui::Rect::from_min_max(
            egui::pos2(full.left() + 10.0, footer_top + 8.0),
            egui::pos2(full.right() - 10.0, full.bottom()),
        );
        paint_text(
            ui,
            footer,
            "LOCAL AUDIO ANALYSIS",
            T_LABEL,
            FAINT,
            Align::Min,
        );
        paint_text(
            ui,
            footer,
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            T_NUM_SMALL,
            FAINT,
            Align::Max,
        );
    }

    fn project_status(&self, project: &Project) -> (String, Color32, Color32) {
        if let Some(worker) = self.worker.as_ref().filter(|w| w.project_id == project.id) {
            return (
                format!("Analyzing {}/{}", worker.completed, worker.total),
                ACCENT_INK,
                ACCENT,
            );
        }
        if project.tracks.is_empty() {
            return ("No tracks yet".into(), FAINT, DISABLED);
        }
        let Some(report) = self.reports.get(&project.id) else {
            return ("Ready to analyze".into(), FAINT, DISABLED);
        };
        let failed = project
            .tracks
            .iter()
            .filter(|t| report.result(t).is_some_and(Result::is_err))
            .count();
        if failed > 0 {
            return (attention(failed), WARNING, WARNING);
        }
        let pending = unmeasured(Some(report), project).count();
        if pending > 0 {
            return (format!("{pending} to analyze"), FAINT, DISABLED);
        }
        if project.mode == AnalysisMode::Album
            && let Some(Ok(album)) = &report.album
        {
            return (
                format!(
                    "All analyzed · {} dB shared",
                    gain(normal_gain(album.integrated_lufs, album.true_peak_dbtp))
                ),
                MUTED,
                ACCENT,
            );
        }
        ("All analyzed".into(), MUTED, ACCENT)
    }

    fn home(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.spacing_mut().item_spacing.y = 0.0;
        let projects = self.library.projects.len();
        let tracks: usize = self.library.projects.iter().map(|p| p.tracks.len()).sum();
        let meta = format!(
            "{} · {} · A place for each release.",
            plural(projects, "project"),
            plural(tracks, "track")
        );
        let header = page_header(
            ui,
            "LIBRARY",
            "All projects",
            &[(meta.as_str(), MUTED)],
            170.0,
        );
        let create = egui::Rect::from_min_size(
            egui::pos2(header.right() - PAD - 128.0, header.bottom() - 46.0),
            Vec2::new(128.0, 30.0),
        );
        if button_at(
            ui,
            "new_project",
            create,
            "New project",
            Some(Icon::Plus),
            self.editable(),
            true,
        )
        .clicked()
        {
            *action = Some(Action::Create);
        }
        hline(ui, header.bottom() - 0.5, LINE);

        if projects == 0 {
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), ui.available_height().max(0.0)),
                Sense::hover(),
            );
            let top = rect.center().y - 90.0;
            paint_logo(
                ui.painter(),
                egui::Rect::from_center_size(
                    egui::pos2(rect.center().x, top + 28.0),
                    Vec2::splat(56.0),
                ),
                MUTED,
                ACCENT,
            );
            paint_text(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(rect.left(), top + 70.0),
                    Vec2::new(rect.width(), 28.0),
                ),
                "Start with a release",
                Type {
                    size: 18.0,
                    ..T_TITLE
                },
                INK,
                Align::Center,
            );
            paint_text(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(rect.left(), top + 100.0),
                    Vec2::new(rect.width(), 20.0),
                ),
                "Create a project, add your music, and analyze its playback level.",
                T_BODY,
                MUTED,
                Align::Center,
            );
            let button = egui::Rect::from_center_size(
                egui::pos2(rect.center().x, top + 148.0),
                Vec2::new(136.0, 32.0),
            );
            if button_at(
                ui,
                "empty_new_project",
                button,
                "New project",
                Some(Icon::Plus),
                self.editable(),
                true,
            )
            .clicked()
            {
                *action = Some(Action::Create);
            }
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("library_grid")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(20.0);
                let gap = 16.0;
                let inner = (ui.available_width() - 2.0 * PAD).max(160.0);
                let columns = (((inner + gap) / (190.0 + gap)).floor() as usize).clamp(1, 6);
                let card_width = (inner - gap * (columns - 1) as f32) / columns as f32;
                let card_height = 212.0;
                let count = projects + 1;
                for start in (0..count).step_by(columns) {
                    let (row, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), card_height),
                        Sense::hover(),
                    );
                    for column in 0..columns.min(count - start) {
                        let index = start + column;
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(
                                row.left() + PAD + column as f32 * (card_width + gap),
                                row.top(),
                            ),
                            Vec2::new(card_width, card_height),
                        );
                        if let Some(project) = self.library.projects.get(index) {
                            if self.project_card(ui, rect, project).clicked() {
                                *action = Some(Action::Open(project.id));
                            }
                        } else if new_project_card(ui, rect, self.editable()).clicked() {
                            *action = Some(Action::Create);
                        }
                    }
                    ui.add_space(gap);
                }
                ui.add_space(8.0);
            });
    }

    fn project_card(
        &self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        project: &Project,
    ) -> egui::Response {
        let response = ui.interact(
            rect,
            ui.id().with(("project_card", project.id)),
            Sense::click(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &project.name)
        });
        let hovered = response.hovered() || response.has_focus();
        let painter = ui.painter();
        painter.rect_filled(
            rect,
            10.0,
            if hovered {
                Color32::from_rgb(0x2E, 0x2D, 0x2A)
            } else {
                RAISED
            },
        );
        painter.rect_stroke(
            rect.shrink(0.5),
            10.0,
            Stroke::new(
                if response.has_focus() { 1.5 } else { 1.0 },
                if response.has_focus() {
                    ACCENT
                } else if hovered {
                    EDGE_HOVER
                } else {
                    LINE
                },
            ),
            egui::StrokeKind::Inside,
        );
        let cover = egui::Rect::from_min_size(
            rect.min + Vec2::splat(10.0),
            Vec2::new(rect.width() - 20.0, 112.0),
        );
        draw_project_cover(ui, cover, project.id);
        let left = rect.left() + 14.0;
        let width = rect.width() - 28.0;
        let mut top = cover.bottom() + 12.0;
        paint_text(
            ui,
            egui::Rect::from_min_size(egui::pos2(left, top), Vec2::new(width, 20.0)),
            &project.name,
            T_CARD,
            INK,
            Align::Min,
        );
        top += 22.0;
        let mode = match project.mode {
            AnalysisMode::Track => "Individual",
            AnalysisMode::Album => "Album",
        };
        paint_text(
            ui,
            egui::Rect::from_min_size(egui::pos2(left, top), Vec2::new(width, 16.0)),
            &format!("{} · {}", plural(project.tracks.len(), "track"), mode),
            T_SMALL,
            MUTED,
            Align::Min,
        );
        top += 26.0;
        let (status, color, pip) = self.project_status(project);
        ui.painter()
            .circle_filled(egui::pos2(left + 3.0, top + 8.0), 3.0, pip);
        paint_text(
            ui,
            egui::Rect::from_min_size(egui::pos2(left + 12.0, top), Vec2::new(width - 12.0, 16.0)),
            &status,
            T_SMALL,
            color,
            Align::Min,
        );
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response.on_hover_text(&project.name)
    }

    fn detail(&self, ui: &mut egui::Ui, project: &Project, action: &mut Option<Action>) {
        ui.spacing_mut().item_spacing.y = 0.0;
        let report = self.reports.get(&project.id);
        let track_count = project.tracks.len();
        let analyzing_here = self
            .worker
            .as_ref()
            .is_some_and(|w| w.project_id == project.id);

        let mut meta: Vec<(String, Color32)> = vec![(plural(track_count, "track"), MUTED)];
        if let Some(report) = report {
            let total: f64 = project
                .tracks
                .iter()
                .filter_map(|t| report.measurement(t))
                .map(|m| m.duration_seconds)
                .sum();
            if total > 0.0 {
                meta.push((duration(total), MUTED));
            }
        }
        if let Some(worker) = &self.worker {
            meta.push(if analyzing_here {
                (
                    format!("Analyzing {}/{}", worker.completed, worker.total),
                    ACCENT_INK,
                )
            } else {
                ("Analyzing another project…".into(), MUTED)
            });
        } else if track_count == 0 {
            meta.push(("No tracks yet".into(), MUTED));
        } else if let Some(report) = report {
            let measured = track_count - unmeasured(Some(report), project).count();
            meta.push(if measured == track_count {
                ("All analyzed".into(), MUTED)
            } else {
                (format!("{measured} analyzed"), MUTED)
            });
        } else {
            meta.push(("Ready to analyze".into(), MUTED));
        }
        if !analyzing_here {
            let needs_attention = project
                .tracks
                .iter()
                .filter(|track| {
                    !track.path.is_file()
                        || report
                            .and_then(|r| r.result(track))
                            .is_some_and(Result::is_err)
                })
                .count();
            if needs_attention > 0 {
                meta.push((attention(needs_attention), WARNING));
            }
        }
        let meta_parts: Vec<(&str, Color32)> = meta.iter().map(|(t, c)| (t.as_str(), *c)).collect();
        let header = page_header(ui, "PROJECT", &project.name, &meta_parts, 290.0);

        let button_top = header.bottom() - 46.0;
        let more_rect = egui::Rect::from_min_size(
            egui::pos2(header.right() - PAD - 30.0, button_top),
            Vec2::splat(30.0),
        );
        let more = icon_at(
            ui,
            "project_menu",
            more_rect,
            Icon::More,
            "Project options",
            self.editable(),
            false,
        );
        egui::Popup::menu(&more).show(|ui| {
            if ui.button("Rename project").clicked() {
                *action = Some(Action::Rename);
                ui.close();
            }
            if ui.button("Delete project…").clicked() {
                *action = Some(Action::Delete);
                ui.close();
            }
        });
        let analyze_rect = egui::Rect::from_min_size(
            egui::pos2(more_rect.left() - 8.0 - 96.0, button_top),
            Vec2::new(96.0, 30.0),
        );
        if let Some(worker) = &self.worker {
            let cancelling = worker.cancelled.load(Ordering::Relaxed);
            if button_at(
                ui,
                "analyze_action",
                analyze_rect,
                if cancelling { "Stopping…" } else { "Cancel" },
                None,
                !cancelling,
                false,
            )
            .clicked()
            {
                *action = Some(Action::Cancel);
            }
        } else {
            // With every track current, the same action measures all of them again.
            let current = track_count > 0 && unmeasured(report, project).next().is_none();
            if button_at(
                ui,
                "analyze_action",
                analyze_rect,
                if current { "Re-analyze" } else { "Analyze" },
                None,
                track_count > 0,
                !current,
            )
            .on_hover_text(if current {
                "Measure every track again."
            } else {
                "Measure tracks that are new, changed, or failed. Other tracks keep their results."
            })
            .clicked()
            {
                *action = Some(Action::Analyze);
            }
        }
        let add_rect = egui::Rect::from_min_size(
            egui::pos2(analyze_rect.left() - 8.0 - 118.0, button_top),
            Vec2::new(118.0, 30.0),
        );
        if button_at(
            ui,
            "add_tracks",
            add_rect,
            "Add tracks",
            Some(Icon::Plus),
            self.editable(),
            false,
        )
        .clicked()
        {
            *action = Some(Action::AddFiles);
        }

        self.mode_bar(ui, project, report, action);

        if project.tracks.is_empty() {
            self.empty_project(ui, action);
        } else {
            self.track_table(ui, project, report, action);
        }
    }

    fn mode_bar(
        &self,
        ui: &mut egui::Ui,
        project: &Project,
        report: Option<&Report>,
        action: &mut Option<Action>,
    ) {
        let (bar, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::hover());
        let center_y = bar.top() + 16.0;
        let mut x = bar.left() + PAD;
        paint_text(
            ui,
            egui::Rect::from_min_size(egui::pos2(x, center_y - 8.0), Vec2::new(40.0, 16.0)),
            "MODE",
            T_LABEL,
            FAINT,
            Align::Min,
        );
        x += 46.0;

        let segmented =
            egui::Rect::from_min_size(egui::pos2(x, center_y - 15.0), Vec2::new(152.0, 30.0));
        ui.painter().rect_filled(segmented, 7.0, SIDEBAR);
        ui.painter().rect_stroke(
            segmented.shrink(0.5),
            7.0,
            Stroke::new(1.0, LINE),
            egui::StrokeKind::Inside,
        );
        let mut segment_left = segmented.left() + 2.0;
        for (mode, text, width, tip) in [
            (
                AnalysisMode::Track,
                "Individual",
                84.0,
                "Normalize each track independently.",
            ),
            (
                AnalysisMode::Album,
                "Album",
                62.0,
                "Use one shared gain to preserve the level differences between tracks.",
            ),
        ] {
            let rect = egui::Rect::from_min_size(
                egui::pos2(segment_left, segmented.top() + 3.0),
                Vec2::new(width, 24.0),
            );
            segment_left += width + 2.0;
            let selected = project.mode == mode;
            let response = ui.interact(rect, ui.id().with(("analysis_mode", text)), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, text)
            });
            let hovered = response.hovered() || response.has_focus();
            if selected {
                ui.painter().rect_filled(rect, 5.0, SELECTED);
            } else if hovered {
                ui.painter().rect_filled(rect, 5.0, RAISED);
            }
            if response.has_focus() {
                ui.painter().rect_stroke(
                    rect,
                    5.0,
                    Stroke::new(1.5, ACCENT),
                    egui::StrokeKind::Inside,
                );
            }
            paint_text(
                ui,
                rect,
                text,
                T_SMALL_MEDIUM,
                if selected || hovered { INK } else { MUTED },
                Align::Center,
            );
            if response.on_hover_text(tip).clicked() && !selected {
                *action = Some(Action::Mode(mode));
            }
        }
        x = segmented.right() + 18.0;

        let info_rect = square_at(bar.right() - PAD - 24.0, center_y, 24.0);
        let limit = info_rect.left() - 12.0;
        match project.mode {
            AnalysisMode::Track => {
                x = paint_text(
                    ui,
                    egui::Rect::from_min_max(
                        egui::pos2(x, center_y - 9.0),
                        egui::pos2(limit, center_y + 9.0),
                    ),
                    "Independent gain per track",
                    T_SMALL,
                    MUTED,
                    Align::Min,
                )
                .right();
            }
            AnalysisMode::Album => match report.and_then(|r| r.album.as_ref()) {
                Some(Ok(album)) => {
                    let loudness = level(album.integrated_lufs);
                    let peak_text = peak(album.true_peak_dbtp);
                    let shared = gain(normal_gain(album.integrated_lufs, album.true_peak_dbtp));
                    for (parts, accent) in [
                        (
                            [
                                ("Album ", MUTED),
                                (loudness.as_str(), INK),
                                (" LUFS", MUTED),
                            ],
                            false,
                        ),
                        (
                            [
                                ("Max peak ", MUTED),
                                (peak_text.as_str(), INK),
                                (" dBTP", MUTED),
                            ],
                            false,
                        ),
                        (
                            [
                                ("Shared gain ", MUTED),
                                (shared.as_str(), ACCENT_INK),
                                (" dB", MUTED),
                            ],
                            true,
                        ),
                    ] {
                        match chip(ui, x, center_y, &parts, accent, limit) {
                            Some(right) => x = right + 6.0,
                            None => break,
                        }
                    }
                }
                Some(Err(error)) => {
                    let response = text_at(
                        ui,
                        egui::Rect::from_min_max(
                            egui::pos2(x, center_y - 9.0),
                            egui::pos2(limit, center_y + 9.0),
                        ),
                        "Album unavailable · Check track errors",
                        T_SMALL,
                        WARNING,
                        Align::Min,
                    );
                    x = limit;
                    response.on_hover_text(error);
                }
                None => {
                    x = paint_text(
                        ui,
                        egui::Rect::from_min_max(
                            egui::pos2(x, center_y - 9.0),
                            egui::pos2(limit, center_y + 9.0),
                        ),
                        "Shared gain · Analyze all tracks first",
                        T_SMALL,
                        MUTED,
                        Align::Min,
                    )
                    .right();
                }
            },
        }

        // Reference model, right-aligned; shortened or hidden when space runs out.
        let available = limit - (x + 16.0);
        for parts in [
            [
                ("Reference  ", FAINT),
                ("Spotify Normal · −14 LUFS · −1 dBTP ceiling", MUTED),
            ],
            [("", FAINT), ("−14 LUFS reference", MUTED)],
        ] {
            let galley = layout_rich(
                ui,
                &parts.map(|(text, color)| (text, T_SMALL, color)),
                f32::INFINITY,
            );
            if galley.size().x <= available {
                paint_galley(
                    ui,
                    egui::Rect::from_min_max(
                        egui::pos2(limit - galley.size().x, center_y - 9.0),
                        egui::pos2(limit, center_y + 9.0),
                    ),
                    galley,
                    Align::Max,
                );
                break;
            }
        }
        let _ = icon_at(
            ui,
            "reference_info",
            info_rect,
            Icon::Info,
            "Reference prediction, not Spotify’s official implementation. Audition applies the estimated gain. Encoding and device processing are not simulated. Album gain uses pooled gated loudness and maximum true peak.",
            true,
            false,
        );

        hline(ui, bar.bottom() - 0.5, LINE);
        if let Some(worker) = self.worker.as_ref().filter(|w| w.project_id == project.id) {
            let fraction = worker.completed as f32 / worker.total.max(1) as f32;
            ui.painter().line_segment(
                [
                    egui::pos2(bar.left(), bar.bottom() - 1.0),
                    egui::pos2(bar.left() + bar.width() * fraction, bar.bottom() - 1.0),
                ],
                Stroke::new(2.0, ACCENT),
            );
        }
    }

    fn empty_project(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ui.available_height().max(0.0)),
            Sense::hover(),
        );
        let top = rect.center().y - 60.0;
        if icon_at(
            ui,
            "empty_add",
            square_at(rect.center().x - 18.0, top, 36.0),
            Icon::Plus,
            "Add tracks",
            self.editable(),
            true,
        )
        .clicked()
        {
            *action = Some(Action::AddFiles);
        }
        paint_text(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.left(), top + 30.0),
                Vec2::new(rect.width(), 26.0),
            ),
            "Drop your music here",
            Type {
                size: 17.0,
                ..T_TITLE
            },
            INK,
            Align::Center,
        );
        paint_text(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.left(), top + 60.0),
                Vec2::new(rect.width(), 18.0),
            ),
            "WAV · FLAC · AIFF · MP3 · AAC · M4A · Ogg",
            T_SMALL,
            MUTED,
            Align::Center,
        );
        paint_text(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.left(), top + 80.0),
                Vec2::new(rect.width(), 18.0),
            ),
            "Files stay in their original location.",
            T_SMALL,
            FAINT,
            Align::Center,
        );
    }

    fn transport(&mut self, ui: &mut egui::Ui, project: &Project, action: &mut Option<Action>) {
        ui.spacing_mut().item_spacing.y = 0.0;
        let state = self.playback.state();
        let index = self
            .audition
            .as_ref()
            .filter(|a| a.project_id == project.id)
            .and_then(|a| project.tracks.iter().position(|t| t.id == a.track_id));
        let report = self.reports.get(&project.id);
        let candidate = project.tracks.get(index.unwrap_or(0));
        let gain_db = report
            .zip(candidate)
            .and_then(|(r, track)| audition_gain(r, project.mode, track));
        let ready = gain_db.is_some();
        let active = matches!(state, PlaybackState::Playing | PlaybackState::Paused);
        let session_id = self.audition.as_ref().map(|a| a.session_id);
        if !active {
            self.seek_preview = None;
        }

        // Seek row: elapsed | rail | total.
        let total = if active || state == PlaybackState::Finished {
            self.playback.duration().as_secs_f64()
        } else {
            report
                .zip(candidate)
                .and_then(|(r, track)| r.measurement(track))
                .map_or(0.0, |m| m.duration_seconds)
        };
        let mut position = self
            .seek_preview
            .unwrap_or_else(|| self.playback.position().as_secs_f64())
            .min(total);
        let (seek_row, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
        paint_text(
            ui,
            egui::Rect::from_min_size(seek_row.min, Vec2::new(44.0, seek_row.height())),
            &playback_time(position),
            T_NUM_SMALL,
            MUTED,
            Align::Max,
        );
        let seek_rect = egui::Rect::from_min_max(
            seek_row.min + Vec2::new(56.0, 0.0),
            egui::pos2(seek_row.right() - 56.0, seek_row.bottom()),
        );
        let mut seek_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("playback_seek", session_id))
                .max_rect(seek_rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        style_slider(&mut seek_ui);
        seek_ui.spacing_mut().slider_width = seek_rect.width();
        let (seek_response, target) = seek_slider(
            &mut seek_ui,
            &mut self.seek_preview,
            &mut position,
            total,
            active,
        );
        let fraction = if total > 0.0 {
            (position / total) as f32
        } else {
            0.0
        };
        paint_slider_handle(&seek_ui, &seek_response, fraction, active);
        if let Some(position) = target
            && let Some(session_id) = session_id
        {
            *action = Some(Action::Seek {
                session_id,
                position,
            });
        }
        paint_text(
            ui,
            egui::Rect::from_min_max(
                egui::pos2(seek_row.right() - 44.0, seek_row.top()),
                seek_row.max,
            ),
            &playback_time(total),
            T_NUM_SMALL,
            MUTED,
            Align::Min,
        );

        ui.add_space(8.0);
        let (row, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::hover());
        let center_y = row.center().y;
        if icon_at(
            ui,
            "transport_previous",
            square_at(row.left(), center_y, 30.0),
            Icon::Previous,
            "Previous track",
            index.is_some_and(|i| i > 0),
            false,
        )
        .clicked()
        {
            *action = Some(Action::Skip(-1));
        }
        let (play_icon, play_tip) = match state {
            PlaybackState::Playing => (Icon::Pause, "Pause"),
            PlaybackState::Loading => (Icon::Stop, "Cancel loading"),
            _ => (Icon::Play, "Play normalized audio"),
        };
        let mut play_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("transport_toggle")
                .max_rect(square_at(row.left() + 34.0, center_y, 38.0)),
        );
        if play_button(
            &mut play_ui,
            play_icon,
            play_tip,
            ready || state == PlaybackState::Loading,
            38.0,
        )
        .clicked()
        {
            *action = Some(Action::TogglePlayback);
        }
        if icon_at(
            ui,
            "transport_stop",
            square_at(row.left() + 76.0, center_y, 30.0),
            Icon::Stop,
            "Stop",
            self.audition.is_some(),
            false,
        )
        .clicked()
        {
            *action = Some(Action::StopPlayback);
        }
        if icon_at(
            ui,
            "transport_next",
            square_at(row.left() + 108.0, center_y, 30.0),
            Icon::Next,
            "Next track",
            index.is_some_and(|i| i + 1 < project.tracks.len()),
            false,
        )
        .clicked()
        {
            *action = Some(Action::Skip(1));
        }

        // Monitor volume, right-aligned.
        let volume_left = row.right() - 206.0;
        if icon_at(
            ui,
            "monitor_mute",
            square_at(volume_left, center_y, 30.0),
            if self.muted {
                Icon::Muted
            } else {
                Icon::Speaker
            },
            if self.muted {
                "Unmute monitor"
            } else {
                "Mute monitor"
            },
            true,
            self.muted,
        )
        .clicked()
        {
            *action = Some(Action::Mute);
        }
        let volume_rect = egui::Rect::from_min_size(
            egui::pos2(volume_left + 38.0, center_y - 10.0),
            Vec2::new(120.0, 20.0),
        );
        let mut volume = self.monitor_volume * 100.0;
        let mut volume_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("monitor_volume")
                .max_rect(volume_rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        style_slider(&mut volume_ui);
        volume_ui.spacing_mut().slider_width = volume_rect.width();
        let volume_response = volume_ui
            .add(egui::Slider::new(&mut volume, 0.0..=100.0).show_value(false))
            .on_hover_text("Monitor volume · Does not change the analysis");
        paint_slider_handle(&volume_ui, &volume_response, volume / 100.0, true);
        if volume_response.changed() {
            *action = Some(Action::Volume(volume / 100.0));
        }
        paint_text(
            ui,
            egui::Rect::from_min_max(egui::pos2(row.right() - 40.0, row.top()), row.max),
            &format!("{:.0}%", self.monitor_volume * 100.0),
            T_NUM_SMALL,
            MUTED,
            Align::Max,
        );

        // Now playing, between the transport buttons and the volume.
        let now = egui::Rect::from_min_max(
            egui::pos2(row.left() + 160.0, row.top()),
            egui::pos2(volume_left - 24.0, row.bottom()),
        );
        if now.width() > 40.0 {
            let title = index
                .map(|i| project.tracks[i].display_name())
                .unwrap_or_else(|| "Normalized preview".into());
            text_at(
                ui,
                egui::Rect::from_min_max(now.min, egui::pos2(now.right(), center_y)),
                &title,
                T_BODY_STRONG,
                INK,
                Align::Min,
            )
            .on_hover_text(&title);
            let gain_text = match project.mode {
                AnalysisMode::Track => format!("Normalized {} dB", gain(gain_db)),
                AnalysisMode::Album => format!("Album gain {} dB", gain(gain_db)),
            };
            let (status, status_color, detail) = match state {
                PlaybackState::Loading => ("Preparing…", MUTED, String::new()),
                PlaybackState::Finished => {
                    ("End of project", MUTED, format!(" · {}", project.name))
                }
                PlaybackState::Paused => (
                    "Paused",
                    ACCENT_INK,
                    format!(" · {gain_text} · {}", project.name),
                ),
                PlaybackState::Playing => (
                    "Playing",
                    ACCENT_INK,
                    format!(" · {gain_text} · {}", project.name),
                ),
                PlaybackState::Stopped => {
                    if ready {
                        ("Ready to listen", MUTED, format!(" · {}", project.name))
                    } else {
                        ("Analyze to enable playback", MUTED, String::new())
                    }
                }
            };
            let galley = layout_rich(
                ui,
                &[
                    (status, T_SMALL_MEDIUM, status_color),
                    (detail.as_str(), T_SMALL, MUTED),
                ],
                now.width(),
            );
            paint_galley(
                ui,
                egui::Rect::from_min_max(
                    egui::pos2(now.left(), center_y + 1.0),
                    now.max - Vec2::new(0.0, 2.0),
                ),
                galley,
                Align::Min,
            );
        }
    }

    fn track_table(
        &self,
        ui: &mut egui::Ui,
        project: &Project,
        report: Option<&Report>,
        action: &mut Option<Action>,
    ) {
        let inset = 16.0;
        let gutter = ui.spacing().scroll.allocated_width();
        let width = (ui.available_width() - 2.0 * inset - gutter).max(320.0);
        // Fixed columns: #, time, LUFS-I, peak, gain, out, menu, plus row padding.
        let fixed = 44.0 + 60.0 + 72.0 + 92.0 + 76.0 + 84.0 + 40.0 + 16.0;
        let show_format = width - fixed - 132.0 >= 180.0;
        let title_width = (width - fixed - if show_format { 132.0 } else { 0.0 }).max(110.0);
        let mut widths = vec![44.0, title_width];
        let mut labels = vec!["#", "TITLE"];
        if show_format {
            widths.push(132.0);
            labels.push("FORMAT");
        }
        widths.extend([60.0, 72.0, 92.0, 76.0, 84.0, 40.0]);
        labels.extend(["TIME", "LUFS-I", "PEAK dBTP", "GAIN dB", "OUT LUFS", ""]);
        let cells = |row: egui::Rect| -> Vec<egui::Rect> {
            let mut x = row.left() + 8.0;
            widths
                .iter()
                .map(|w| {
                    let cell = egui::Rect::from_min_max(
                        egui::pos2(x, row.top()),
                        egui::pos2(x + w, row.bottom()),
                    );
                    x += w;
                    cell
                })
                .collect()
        };
        let numeric_start = if show_format { 3 } else { 2 };
        let last = widths.len() - 1;

        let (header_full, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
        let header = egui::Rect::from_min_size(
            header_full.min + Vec2::new(inset, 0.0),
            Vec2::new(width, header_full.height()),
        );
        for (column, (cell, label)) in cells(header).into_iter().zip(&labels).enumerate() {
            let align = if column == 0 {
                Align::Center
            } else if column >= numeric_start && column < last {
                Align::Max
            } else {
                Align::Min
            };
            paint_text(
                ui,
                cell.shrink2(Vec2::new(8.0, 0.0)),
                label,
                Type {
                    tracking: 0.4,
                    ..T_LABEL
                },
                FAINT,
                align,
            );
        }
        ui.painter().line_segment(
            [
                egui::pos2(header.left(), header.bottom() - 0.5),
                egui::pos2(header.right(), header.bottom() - 0.5),
            ],
            Stroke::new(1.0, LINE),
        );

        ui.spacing_mut().item_spacing.y = 0.0;
        let drag_enabled = self.editable() && self.dialog.is_none() && project.tracks.len() > 1;
        let viewport = ui.available_rect_before_wrap();
        let scroll = egui::ScrollArea::vertical()
            .id_salt(("tracks", project.id))
            .auto_shrink([false, false])
            .show_rows(ui, ROW_HEIGHT, project.tracks.len(), |ui, rows| {
                let content_top = ui.cursor().top() - rows.start as f32 * ROW_HEIGHT;
                for index in rows {
                    let track = &project.tracks[index];
                    ui.push_id(track.id, |ui| {
                        let (row_full, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), ROW_HEIGHT),
                            Sense::hover(),
                        );
                        let row = egui::Rect::from_min_size(
                            row_full.min + Vec2::new(inset, 0.0),
                            Vec2::new(width, ROW_HEIGHT),
                        );
                        let hovered = ui.rect_contains_pointer(row);
                        let current = self
                            .audition
                            .as_ref()
                            .is_some_and(|a| a.track_id == track.id);
                        let state = self.playback.state();
                        let playing = current && state == PlaybackState::Playing;
                        let dragging = track_drag::is_dragging(ui.ctx(), project.id, track.id);
                        let fill = if dragging {
                            SELECTED
                        } else if current {
                            ACCENT_TINT
                        } else if hovered {
                            RAISED
                        } else {
                            Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(row, 4.0, fill);
                        ui.painter().line_segment(
                            [
                                egui::pos2(row.left(), row.bottom() - 0.5),
                                egui::pos2(row.right(), row.bottom() - 0.5),
                            ],
                            Stroke::new(1.0, HAIRLINE),
                        );

                        let result = report.and_then(|r| r.result(track));
                        let measurement = report.and_then(|r| r.measurement(track));
                        let missing = !track.path.is_file();
                        let applied_gain =
                            report.and_then(|r| audition_gain(r, project.mode, track));
                        let output = measurement
                            .and_then(|m| m.integrated_lufs)
                            .zip(applied_gain)
                            .map(|(l, g)| l + g);
                        let cell = cells(row);
                        let payload = TrackDrag {
                            project_id: project.id,
                            track_id: track.id,
                        };
                        // Keep playback and menu controls separate from the drag surface.
                        track_drag::source(
                            ui,
                            egui::Rect::from_min_max(cell[1].min, cell[last].left_bottom()),
                            ui.id().with("track_drag"),
                            payload,
                            drag_enabled,
                        );
                        let grip = egui::Rect::from_min_max(
                            egui::pos2(row.left() - inset, row.top()),
                            egui::pos2(row.left() + 8.0, row.bottom()),
                        );
                        let grip_response = track_drag::source(
                            ui,
                            grip,
                            ui.id().with("track_grip"),
                            payload,
                            drag_enabled,
                        );
                        if drag_enabled && (hovered || grip_response.hovered() || dragging) {
                            for x in [-2.5, 2.5] {
                                for y in [-5.0, 0.0, 5.0] {
                                    ui.painter().circle_filled(
                                        grip.center() + Vec2::new(x, y),
                                        1.0,
                                        if dragging { ACCENT_INK } else { FAINT },
                                    );
                                }
                            }
                        }

                        // Index / play toggle.
                        let error = if missing {
                            Some("File unavailable · Replace file to relink".to_string())
                        } else if let Some(Err(error)) = result {
                            Some(error.clone())
                        } else {
                            None
                        };
                        if let Some(error) = &error {
                            let icon =
                                egui::Rect::from_center_size(cell[0].center(), Vec2::splat(16.0));
                            paint_icon(ui.painter(), icon, Icon::Warning, WARNING);
                            ui.interact(icon, ui.id().with("warning"), Sense::hover())
                                .on_hover_text(error);
                        } else {
                            let number = format!("{}", index + 1);
                            let glyph = if playing {
                                RowGlyph::Equalizer
                            } else {
                                RowGlyph::Number(&number, if current { ACCENT_INK } else { FAINT })
                            };
                            let enabled = applied_gain.is_some();
                            let toggle_id = ui.id().with("toggle");
                            let response = row_toggle(
                                ui,
                                toggle_id,
                                egui::Rect::from_center_size(cell[0].center(), Vec2::splat(28.0)),
                                glyph,
                                if playing { Icon::Pause } else { Icon::Play },
                                enabled,
                                if !enabled {
                                    "Analyze to enable playback"
                                } else if playing {
                                    "Pause"
                                } else {
                                    "Play normalized track"
                                },
                                hovered,
                            );
                            if response.clicked() {
                                *action = Some(
                                    if current
                                        && matches!(
                                            state,
                                            PlaybackState::Playing | PlaybackState::Paused
                                        )
                                    {
                                        Action::TogglePlayback
                                    } else {
                                        Action::PlayTrack(track.id)
                                    },
                                );
                            }
                        }

                        // Title, with an inline status when something needs attention.
                        let title_cell = cell[1].shrink2(Vec2::new(8.0, 0.0));
                        let name = track.display_name();
                        let (name_type, name_color) = if error.is_some() {
                            (T_BODY, MUTED)
                        } else if current {
                            (T_BODY_STRONG, ACCENT_INK)
                        } else {
                            (T_BODY, INK)
                        };
                        let name_rect =
                            paint_text(ui, title_cell, &name, name_type, name_color, Align::Min);
                        ui.interact(name_rect, ui.id().with("name"), Sense::hover())
                            .on_hover_text(track.path.to_string_lossy());
                        let note = error.clone().map(|e| (e, WARNING)).or_else(|| {
                            measurement
                                .filter(|m| m.integrated_lufs.is_none())
                                .map(|_| ("No measurable loudness".to_string(), MUTED))
                        });
                        if let Some((note, color)) = note {
                            let note_rect = egui::Rect::from_min_max(
                                egui::pos2(name_rect.right() + 10.0, title_cell.top()),
                                title_cell.max,
                            );
                            if note_rect.width() > 24.0 {
                                text_at(ui, note_rect, &note, T_SMALL, color, Align::Min)
                                    .on_hover_text(&note);
                            }
                        }

                        let mut column = 2;
                        if show_format {
                            let format = measurement.map_or_else(
                                || "—".to_string(),
                                |m| {
                                    format!(
                                        "{} · {}",
                                        sample_rate(m.sample_rate),
                                        channels(m.channels)
                                    )
                                },
                            );
                            paint_text(
                                ui,
                                cell[column].shrink2(Vec2::new(8.0, 0.0)),
                                &format,
                                T_SMALL,
                                if measurement.is_some() { MUTED } else { FAINT },
                                Align::Min,
                            );
                            column += 1;
                        }
                        let values = [
                            (
                                measurement
                                    .map_or_else(|| "—".into(), |m| duration(m.duration_seconds)),
                                INK,
                            ),
                            (level(measurement.and_then(|m| m.integrated_lufs)), INK),
                            (peak(measurement.and_then(|m| m.true_peak_dbtp)), INK),
                            (gain(applied_gain), ACCENT_INK),
                            (level(output), INK),
                        ];
                        for (text, color) in values {
                            paint_text(
                                ui,
                                cell[column].shrink2(Vec2::new(8.0, 0.0)),
                                &text,
                                T_NUM,
                                if text == "—" { FAINT } else { color },
                                Align::Max,
                            );
                            column += 1;
                        }

                        let menu_rect =
                            egui::Rect::from_center_size(cell[last].center(), Vec2::splat(26.0));
                        let more = icon_at_reveal(
                            ui,
                            "track_menu",
                            menu_rect,
                            Icon::More,
                            "Track options",
                            self.editable(),
                            hovered || error.is_some(),
                        );
                        if egui::Popup::is_id_open(
                            ui.ctx(),
                            egui::Popup::default_response_id(&more),
                        ) {
                            ui.painter().rect_filled(menu_rect, 6.0, SELECTED);
                            paint_icon(
                                ui.painter(),
                                egui::Rect::from_center_size(menu_rect.center(), Vec2::splat(16.0)),
                                Icon::More,
                                INK,
                            );
                        }
                        egui::Popup::menu(&more).show(|ui| {
                            if ui.button("Replace file…").clicked() {
                                *action = Some(Action::Replace(track.id));
                                ui.close();
                            }
                            if ui
                                .add_enabled(index > 0, egui::Button::new("Move up"))
                                .clicked()
                            {
                                *action = Some(Action::Move(track.id, -1));
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    index + 1 < project.tracks.len(),
                                    egui::Button::new("Move down"),
                                )
                                .clicked()
                            {
                                *action = Some(Action::Move(track.id, 1));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Remove from project").clicked() {
                                *action = Some(Action::Remove(track.id));
                                ui.close();
                            }
                        });
                    });
                }
                track_drag::auto_scroll(
                    ui,
                    project.id,
                    viewport.intersect(ui.clip_rect()),
                    drag_enabled,
                );
                content_top
            });
        if let Some((track_id, insert_before)) = track_drag::drop_target(
            ui,
            project,
            scroll.inner_rect,
            scroll.inner,
            ROW_HEIGHT,
            drag_enabled,
        ) {
            *action = Some(Action::Reorder {
                project_id: project.id,
                track_id,
                insert_before,
            });
        }
    }

    fn dispatch(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Open(id) => {
                self.stop_playback();
                self.selected = Some(id);
                if !self.read_only {
                    self.notice = None;
                }
            }
            Action::Back => {
                self.stop_playback();
                self.selected = None;
            }
            Action::Create => self.dialog = Some(Dialog::Create(String::new())),
            Action::AddFiles => {
                if let Some(paths) = audio_dialog().pick_files() {
                    self.add_paths(paths);
                }
            }
            Action::Replace(id) => {
                if let Some(path) = audio_dialog().pick_file() {
                    self.edit_project(|p| {
                        if let Some(track) = p.tracks.iter_mut().find(|t| t.id == id) {
                            track.path = path;
                        }
                    });
                }
            }
            Action::Remove(id) => self.edit_project(|p| p.tracks.retain(|t| t.id != id)),
            Action::Move(id, delta) => self.move_track(id, delta),
            Action::Reorder {
                project_id,
                track_id,
                insert_before,
            } => {
                self.reorder_track(project_id, track_id, insert_before);
            }
            Action::Mode(mode) => {
                if let Some(id) = self.selected
                    && self.commit(|library| {
                        if let Some(p) = library.projects.iter_mut().find(|p| p.id == id) {
                            p.mode = mode;
                        }
                    })
                    && self.sync_playback_gain()
                {
                    self.notice =
                        Some("Playback stopped: normalization is unavailable in this mode.".into());
                }
            }
            Action::PlayTrack(id) => self.start_playback(id),
            Action::TogglePlayback => self.toggle_playback(),
            Action::StopPlayback => self.stop_playback(),
            Action::Skip(delta) => self.skip_playback(delta),
            Action::Seek {
                session_id,
                position,
            } => {
                if self
                    .audition
                    .as_ref()
                    .is_some_and(|a| a.session_id == session_id)
                    && let Err(error) = self.playback.seek(position)
                {
                    self.notice = Some(format!("Cannot seek: {error}"));
                }
            }
            Action::Volume(volume) => {
                self.monitor_volume = volume.clamp(0.0, 1.0);
                self.playback
                    .set_volume(if self.muted { 0.0 } else { self.monitor_volume });
            }
            Action::Mute => {
                self.muted = !self.muted;
                self.playback
                    .set_volume(if self.muted { 0.0 } else { self.monitor_volume });
            }
            Action::Analyze => self.start_analysis(ctx),
            Action::Cancel => {
                if let Some(worker) = &self.worker {
                    worker.cancelled.store(true, Ordering::Relaxed);
                }
            }
            Action::Rename => {
                if let Some(p) = self.project() {
                    self.dialog = Some(Dialog::Rename(p.id, p.name.clone()));
                }
            }
            Action::Delete => {
                if let Some(id) = self.selected {
                    self.dialog = Some(Dialog::Delete(id));
                }
            }
        }
    }

    fn show_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let title = match &dialog {
            Dialog::Create(_) => "New project",
            Dialog::Rename(_, _) => "Rename project",
            Dialog::Delete(_) => "Delete project?",
        };
        let mut accept = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("project_dialog")).show(ctx, |ui| {
            ui.set_width(360.0);
            let heading = layout_text(ui, title, T_BRAND, INK, 360.0);
            ui.label(heading);
            ui.add_space(14.0);
            match &mut dialog {
                Dialog::Create(name) | Dialog::Rename(_, name) => {
                    ui.label("Project name");
                    let response = ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY).hint_text("e.g. Night Sessions"));
                    if !ctx.memory(|m| m.has_focus(response.id)) && !ui.input(|i| i.pointer.any_down()) { response.request_focus(); }
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        accept = ui.add_enabled(!name.trim().is_empty(), primary_button("Save project")).clicked()
                            || (!name.trim().is_empty() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                        cancel = ui.button("Cancel").clicked();
                    });
                }
                Dialog::Delete(_) => {
                    ui.label("Remove this project from LoudTrace? Your original audio files will stay where they are.");
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        accept = ui.button("Delete project").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                }
            }
            cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
        });
        if accept {
            match dialog {
                Dialog::Create(name) => {
                    let project = Project::new(name.trim().to_owned());
                    let id = project.id;
                    if self.commit(|l| l.projects.push(project)) {
                        self.selected = Some(id);
                    }
                }
                Dialog::Rename(id, name) => {
                    self.commit(|l| {
                        if let Some(p) = l.projects.iter_mut().find(|p| p.id == id) {
                            p.name = name.trim().to_owned();
                        }
                    });
                }
                Dialog::Delete(id) => {
                    if self.commit(|l| l.projects.retain(|p| p.id != id)) {
                        self.stop_playback();
                        self.reports.remove(&id);
                        self.selected = None;
                    }
                }
            }
        } else if !cancel {
            self.dialog = Some(dialog);
        }
    }
}

impl eframe::App for LoudTraceApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();
        self.check_changed_files();
        self.poll_playback();
        ctx.request_repaint_after(
            if matches!(
                self.playback.state(),
                PlaybackState::Loading | PlaybackState::Playing
            ) {
                Duration::from_millis(33)
            } else {
                Duration::from_secs(1)
            },
        );
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut action = None;
        let project = self.project().cloned();
        // The transport spans the full window width, VLC-style, so it is laid out first.
        if let Some(project) = &project {
            egui::Panel::bottom("playback_footer")
                .exact_size(96.0)
                .resizable(false)
                .show_separator_line(true)
                .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(egui::Margin {
                    left: 20,
                    right: 20,
                    top: 12,
                    bottom: 12,
                }))
                .show(ui, |ui| {
                    self.transport(ui, project, &mut action);
                });
        }
        egui::Panel::left("library_sidebar")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .show_separator_line(true)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(egui::Margin {
                left: 10,
                right: 10,
                top: 14,
                bottom: 12,
            }))
            .show(ui, |ui| {
                self.sidebar(ui, &mut action);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| {
                if let Some(notice) = &self.notice {
                    notice_banner(ui, notice);
                }
                if let Some(project) = &project {
                    self.detail(ui, project, &mut action);
                } else {
                    self.home(ui, &mut action);
                }
            });
        if self.dialog.is_none() {
            if let Some(action) = action {
                self.dispatch(action, &ctx);
            }
            if self.selected.is_some() && self.editable() {
                let dropped = ctx.input(|i| {
                    i.raw
                        .dropped_files
                        .iter()
                        .map(|f| f.path().to_path_buf())
                        .collect::<Vec<_>>()
                });
                self.add_paths(dropped);
            }
        }
        self.show_dialog(&ctx);
    }

    fn on_exit(&mut self) {
        self.stop_playback();
        if let Some(worker) = &self.worker {
            worker.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

// Tracks without a current measurement: new, relinked, changed, or failed.
fn unmeasured<'a>(
    report: Option<&'a Report>,
    project: &'a Project,
) -> impl Iterator<Item = &'a Track> {
    project
        .tracks
        .iter()
        .filter(move |track| report.and_then(|r| r.measurement(track)).is_none())
}

fn audition_gain(report: &Report, mode: AnalysisMode, track: &Track) -> Option<f64> {
    let measurement = report.measurement(track)?;
    match mode {
        AnalysisMode::Track => normal_gain(measurement.integrated_lufs, measurement.true_peak_dbtp),
        AnalysisMode::Album => {
            let album = report.album.as_ref()?.as_ref().ok()?;
            normal_gain(album.integrated_lufs, album.true_peak_dbtp)
        }
    }
}

fn audio_dialog() -> rfd::FileDialog {
    rfd::FileDialog::new()
        .set_title("Choose audio files")
        .add_filter(
            "Audio",
            &[
                "wav", "wave", "flac", "aif", "aiff", "mp3", "m4a", "aac", "ogg", "oga", "caf",
            ],
        )
        .add_filter("All files", &["*"])
}

fn seek_slider(
    ui: &mut egui::Ui,
    preview: &mut Option<f64>,
    position: &mut f64,
    total: f64,
    active: bool,
) -> (egui::Response, Option<Duration>) {
    let response = ui.add_enabled(
        active,
        egui::Slider::new(position, 0.0..=total.max(0.001)).show_value(false),
    );
    let target = if !active || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        *preview = None;
        None
    } else if response.is_pointer_button_down_on() {
        // Keep the preview independent of the advancing playhead. Seek only
        // once on release, including a release outside the slider bounds.
        *preview = Some(*position);
        None
    } else if response.drag_stopped() || response.changed() {
        // Non-pointer changes (keyboard/accessibility) still apply immediately.
        *preview = None;
        Some(Duration::from_secs_f64(*position))
    } else {
        *preview = None;
        None
    };
    (response, target)
}

const SIDEBAR_WIDTH: f32 = 232.0;
const PAD: f32 = 24.0;
const ROW_HEIGHT: f32 = 40.0;
const HEADER_HEIGHT: f32 = 100.0;

fn square_at(left: f32, center_y: f32, size: f32) -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(left, center_y - size / 2.0), Vec2::splat(size))
}

fn icon_at(
    ui: &mut egui::Ui,
    id: &str,
    rect: egui::Rect,
    icon: Icon,
    tooltip: &str,
    enabled: bool,
    active: bool,
) -> egui::Response {
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(rect));
    icon_button(&mut child, icon, tooltip, enabled, active, rect.height())
}

fn icon_at_reveal(
    ui: &mut egui::Ui,
    id: &str,
    rect: egui::Rect,
    icon: Icon,
    tooltip: &str,
    enabled: bool,
    reveal: bool,
) -> egui::Response {
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(rect));
    icon_button_reveal(
        &mut child,
        icon,
        tooltip,
        enabled,
        false,
        rect.height(),
        reveal,
    )
}

/// A text button drawn to the LoudTrace style. `primary` uses the clay fill.
fn button_at(
    ui: &mut egui::Ui,
    id: &str,
    rect: egui::Rect,
    text: &str,
    icon: Option<Icon>,
    enabled: bool,
    primary: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(("button", id)),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    let hovered = enabled && (response.hovered() || response.has_focus());
    let pressed = enabled && response.is_pointer_button_down_on();
    let painter = ui.painter();
    let (fill, stroke, color) = match (primary, enabled) {
        (true, true) => (
            if pressed {
                ACCENT_PRESSED
            } else if hovered {
                ACCENT_HOVER
            } else {
                ACCENT
            },
            Stroke::NONE,
            SIDEBAR,
        ),
        (true, false) => (Color32::from_rgb(0x5C, 0x3E, 0x32), Stroke::NONE, MUTED),
        (false, true) => (
            if pressed {
                RAIL
            } else if hovered {
                SELECTED
            } else {
                RAISED
            },
            Stroke::new(1.0, if hovered { EDGE_HOVER } else { LINE }),
            INK,
        ),
        (false, false) => (RAISED, Stroke::new(1.0, LINE), DISABLED),
    };
    painter.rect_filled(rect, 6.0, fill);
    if stroke.width > 0.0 {
        painter.rect_stroke(rect.shrink(0.5), 6.0, stroke, egui::StrokeKind::Inside);
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(2.0),
            8.0,
            Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    let ty = if primary {
        T_BODY_STRONG
    } else {
        T_BODY_MEDIUM
    };
    let galley = layout_text(ui, text, ty, color, rect.width() - 16.0);
    let icon_width = if icon.is_some() { 20.0 } else { 0.0 };
    let content = galley.size().x + icon_width;
    let left = rect.center().x - content / 2.0;
    if let Some(icon) = icon {
        paint_icon(
            painter,
            egui::Rect::from_center_size(
                egui::pos2(left + 8.0, rect.center().y),
                Vec2::splat(16.0),
            ),
            icon,
            color,
        );
    }
    painter.galley(
        egui::pos2(left + icon_width, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    response
}

/// Paint a prepared galley inside `rect`; returns the painted bounds.
fn paint_galley(
    ui: &egui::Ui,
    rect: egui::Rect,
    galley: Arc<egui::text::Galley>,
    align: Align,
) -> egui::Rect {
    let left = match align {
        Align::Min => rect.left(),
        Align::Center => rect.center().x - galley.size().x / 2.0,
        Align::Max => rect.right() - galley.size().x,
    };
    let position = egui::pos2(left, rect.center().y - galley.size().y / 2.0);
    let painted = egui::Rect::from_min_size(position, galley.size());
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(position, galley, INK);
    painted
}

/// Paint one line of text inside `rect`; returns the painted bounds.
fn paint_text(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    ty: Type,
    color: Color32,
    align: Align,
) -> egui::Rect {
    let galley = layout_text(ui, text, ty, color, rect.width());
    paint_galley(ui, rect, galley, align)
}

/// Paint text and return a hover response for tooltips.
fn text_at(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    ty: Type,
    color: Color32,
    align: Align,
) -> egui::Response {
    paint_text(ui, rect, text, ty, color, align);
    ui.interact(
        rect,
        ui.id()
            .with(("label", rect.left().to_bits(), rect.top().to_bits())),
        Sense::hover(),
    )
}

/// Page header: small caps label, title, and a metadata line. Leaves `reserve_right` for buttons.
fn page_header(
    ui: &mut egui::Ui,
    label: &str,
    title: &str,
    meta: &[(&str, Color32)],
    reserve_right: f32,
) -> egui::Rect {
    let (header, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), HEADER_HEIGHT),
        Sense::hover(),
    );
    let left = header.left() + PAD;
    let right = (header.right() - PAD - reserve_right).max(left + 40.0);
    paint_text(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(left, header.top() + 20.0),
            egui::pos2(right, header.top() + 34.0),
        ),
        label,
        T_LABEL,
        FAINT,
        Align::Min,
    );
    text_at(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(left, header.top() + 37.0),
            egui::pos2(right, header.top() + 65.0),
        ),
        title,
        T_TITLE,
        INK,
        Align::Min,
    )
    .on_hover_text(title);
    let mut parts: Vec<(&str, Type, Color32)> = Vec::new();
    for (index, (text, color)) in meta.iter().enumerate() {
        if index > 0 {
            parts.push((" · ", T_SMALL, FAINT));
        }
        parts.push((*text, T_SMALL, *color));
    }
    let galley = layout_rich(ui, &parts, right - left);
    paint_galley(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(left, header.top() + 68.0),
            egui::pos2(right, header.top() + 84.0),
        ),
        galley,
        Align::Min,
    );
    header
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    paint_text(
        ui,
        egui::Rect::from_min_max(rect.min + Vec2::new(10.0, 0.0), rect.max),
        text,
        T_LABEL,
        FAINT,
        Align::Min,
    );
}

/// A full-width horizontal rule across the current panel.
fn hline(ui: &egui::Ui, y: f32, color: Color32) {
    let rect = ui.max_rect();
    ui.painter().line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        Stroke::new(1.0, color),
    );
}

/// Sidebar navigation row.
fn nav_item(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    count: Option<usize>,
    selected: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    let hovered = response.hovered() || response.has_focus();
    let fill = if selected {
        SELECTED
    } else if hovered {
        RAISED
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(0.5),
            6.0,
            Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 18.0, rect.center().y),
            Vec2::splat(16.0),
        ),
        icon,
        if selected {
            ACCENT
        } else if hovered {
            INK
        } else {
            MUTED
        },
    );
    let mut right = rect.right() - 10.0;
    if let Some(count) = count {
        let painted = paint_text(
            ui,
            egui::Rect::from_min_max(
                egui::pos2(right - 40.0, rect.top()),
                egui::pos2(right, rect.bottom()),
            ),
            &count.to_string(),
            T_NUM_SMALL,
            FAINT,
            Align::Max,
        );
        right = painted.left() - 8.0;
    }
    paint_text(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 36.0, rect.top()),
            egui::pos2(right, rect.bottom()),
        ),
        label,
        if selected { T_BODY_MEDIUM } else { T_BODY },
        if selected || hovered { INK } else { MUTED },
        Align::Min,
    );
    response
}

fn new_project_card(ui: &mut egui::Ui, rect: egui::Rect, enabled: bool) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with("new_project_card"),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, "New project")
    });
    let hovered = enabled && (response.hovered() || response.has_focus());
    if hovered {
        ui.painter().rect_filled(rect, 10.0, RAISED);
    }
    ui.painter().rect_stroke(
        rect.shrink(0.5),
        10.0,
        Stroke::new(
            1.0,
            if response.has_focus() {
                ACCENT
            } else if hovered {
                EDGE_HOVER
            } else {
                LINE
            },
        ),
        egui::StrokeKind::Inside,
    );
    let color = if !enabled {
        DISABLED
    } else if hovered {
        INK
    } else {
        MUTED
    };
    let center = rect.center();
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(center - Vec2::new(0.0, 22.0), Vec2::splat(20.0)),
        Icon::Plus,
        color,
    );
    paint_text(
        ui,
        egui::Rect::from_center_size(
            center + Vec2::new(0.0, 6.0),
            Vec2::new(rect.width() - 24.0, 18.0),
        ),
        "New project",
        T_BODY_MEDIUM,
        color,
        Align::Center,
    );
    paint_text(
        ui,
        egui::Rect::from_center_size(
            center + Vec2::new(0.0, 26.0),
            Vec2::new(rect.width() - 24.0, 16.0),
        ),
        "A place for each release.",
        T_SMALL,
        FAINT,
        Align::Center,
    );
    response
}

/// A rounded summary chip. Returns its right edge, or `None` if it would cross `limit`.
fn chip(
    ui: &egui::Ui,
    left: f32,
    center_y: f32,
    parts: &[(&str, Color32)],
    accent: bool,
    limit: f32,
) -> Option<f32> {
    let styled: Vec<(&str, Type, Color32)> = parts
        .iter()
        .map(|(text, color)| {
            let ty = if *color == MUTED {
                T_SMALL
            } else {
                T_NUM_SMALL
            };
            (*text, ty, *color)
        })
        .collect();
    let galley = layout_rich(ui, &styled, f32::INFINITY);
    let rect = egui::Rect::from_min_size(
        egui::pos2(left, center_y - 12.0),
        Vec2::new(galley.size().x + 20.0, 24.0),
    );
    if rect.right() > limit {
        return None;
    }
    let painter = ui.painter();
    painter.rect_filled(rect, 12.0, if accent { ACCENT_TINT } else { RAISED });
    painter.rect_stroke(
        rect.shrink(0.5),
        12.0,
        Stroke::new(1.0, if accent { ACCENT_EDGE } else { LINE }),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        egui::pos2(rect.left() + 10.0, center_y - galley.size().y / 2.0),
        galley,
        INK,
    );
    Some(rect.right())
}

fn notice_banner(ui: &mut egui::Ui, notice: &str) {
    ui.add_space(12.0);
    let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::hover());
    let rect = outer.shrink2(Vec2::new(PAD, 0.0));
    ui.painter().rect_filled(rect, 8.0, WARNING_TINT);
    ui.painter().rect_stroke(
        rect.shrink(0.5),
        8.0,
        Stroke::new(1.0, WARNING_EDGE),
        egui::StrokeKind::Inside,
    );
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            Vec2::splat(16.0),
        ),
        Icon::Warning,
        WARNING,
    );
    text_at(
        ui,
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 38.0, rect.top()),
            egui::pos2(rect.right() - 12.0, rect.bottom()),
        ),
        notice,
        T_SMALL,
        INK,
        Align::Min,
    )
    .on_hover_text(notice);
}

enum RowGlyph<'a> {
    Number(&'a str, Color32),
    Equalizer,
}

/// The track-number cell: shows the number (or a playing glyph) and turns into a
/// play/pause button while the row is hovered or focused.
#[allow(clippy::too_many_arguments)]
fn row_toggle(
    ui: &mut egui::Ui,
    id: egui::Id,
    rect: egui::Rect,
    glyph: RowGlyph<'_>,
    hover_icon: Icon,
    enabled: bool,
    tooltip: &str,
    row_hovered: bool,
) -> egui::Response {
    let response = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tooltip));
    let focused = response.has_focus();
    if enabled && (response.hovered() || focused) {
        ui.painter().rect_filled(rect, 6.0, SELECTED);
    }
    if focused {
        ui.painter().rect_stroke(
            rect.shrink(0.5),
            6.0,
            Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    let icon_rect = egui::Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    if enabled && (row_hovered || focused) {
        paint_icon(ui.painter(), icon_rect, hover_icon, INK);
    } else {
        match glyph {
            RowGlyph::Number(text, color) => {
                paint_text(ui, rect, text, T_NUM_SMALL, color, Align::Center);
            }
            RowGlyph::Equalizer => paint_icon(ui.painter(), icon_rect, Icon::Equalizer, ACCENT),
        }
    }
    response.on_hover_text(tooltip)
}

fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(SIDEBAR).strong())
        .fill(ACCENT)
        .min_size(Vec2::new(110.0, 30.0))
        .corner_radius(6)
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn attention(count: usize) -> String {
    if count == 1 {
        "1 needs attention".into()
    } else {
        format!("{count} need attention")
    }
}

/// One decimal, with a true minus sign; `plus` adds "+" to positive values.
fn signed(value: f64, plus: bool) -> String {
    let value = if value.abs() < 0.05 { 0.0 } else { value };
    let digits = format!("{:.1}", value.abs());
    if value < 0.0 {
        format!("−{digits}")
    } else if plus && value > 0.0 {
        format!("+{digits}")
    } else {
        digits
    }
}

fn level(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map_or_else(|| "—".into(), |v| signed(v, false))
}

fn peak(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map_or_else(|| "—".into(), |v| signed(v, true))
}

fn gain(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map_or_else(|| "—".into(), |v| signed(v, true))
}

fn sample_rate(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", hz as f64 / 1000.0)
    }
}

fn channels(count: u32) -> String {
    match count {
        1 => "Mono".into(),
        2 => "Stereo".into(),
        n => format!("{n} ch"),
    }
}

fn playback_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0).floor() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn duration(seconds: f64) -> String {
    let seconds = seconds.round() as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// Project cover: a waveform seeded by the project id, held between two limiter bounds.
fn draw_project_cover(ui: &egui::Ui, rect: egui::Rect, id: Uuid) {
    let bytes = id.as_bytes();
    let tint = Color32::from_rgb(
        0x28 + bytes[0] % 8,
        0x24 + bytes[1] % 6,
        0x20 + bytes[2] % 5,
    );
    let painter = ui.painter();
    painter.rect_filled(rect, 7.0, tint);
    let inner = rect.shrink(14.0);
    for y in [inner.top(), inner.bottom()] {
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(inner.left(), y - 1.0),
                egui::pos2(inner.right(), y + 1.0),
            ),
            1.0,
            ACCENT.gamma_multiply(0.6),
        );
    }
    let wave = inner.shrink2(Vec2::new(6.0, 8.0));
    let bars = 24;
    let step = wave.width() / (bars - 1) as f32;
    for i in 0..bars {
        let t = i as f32 / (bars - 1) as f32;
        let envelope = (t * std::f32::consts::PI).sin().powf(0.6);
        let seed = bytes[(i * 7 + 3) % 16] as f32 / 255.0;
        let height = (4.0 + envelope * (12.0 + seed * 44.0)).min(wave.height());
        let bar = egui::Rect::from_center_size(
            egui::pos2(wave.left() + i as f32 * step, wave.center().y),
            Vec2::new(3.0, height),
        );
        painter.rect_filled(bar, 1.5, INK.gamma_multiply(0.35 + seed * 0.5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> (tempfile::TempDir, LoudTraceApp) {
        let dir = tempfile::tempdir().unwrap();
        let app = LoudTraceApp::load(ProjectStore::new(dir.path().join("projects.json")));
        (dir, app)
    }

    fn seek_frame(
        ctx: &egui::Context,
        preview: &mut Option<f64>,
        playhead: f64,
        session: u32,
        events: Vec<egui::Event>,
    ) -> (egui::Response, Option<Duration>, f64) {
        let mut position = preview.unwrap_or(playhead);
        let mut result = None;
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(400.0, 100.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                ui.push_id(session, |ui| {
                    result = Some(seek_slider(ui, preview, &mut position, 100.0, true));
                });
            },
        )
        .drop_without_applying_deltas();
        let (response, target) = result.unwrap();
        (response, target, position)
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn key_press(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn seek_drag_previews_until_release_then_commits_once() {
        let ctx = egui::Context::default();
        crate::ui_style::configure_style(&ctx).expect("system fonts");
        let mut preview = None;
        let (response, _, _) = seek_frame(&ctx, &mut preview, 5.0, 1, vec![]);
        let start = response.rect.center();
        let end = egui::pos2(response.rect.right() - 15.0, start.y);
        let (_, target, _) = seek_frame(
            &ctx,
            &mut preview,
            6.0,
            1,
            vec![
                egui::Event::PointerMoved(start),
                pointer_button(start, true),
            ],
        );
        assert!(target.is_none());
        assert!(preview.is_some());
        let (_, target, dragged_position) = seek_frame(
            &ctx,
            &mut preview,
            7.0,
            1,
            vec![egui::Event::PointerMoved(end)],
        );
        assert!(target.is_none());
        let (_, target, held_position) = seek_frame(&ctx, &mut preview, 8.0, 1, vec![]);
        assert!(target.is_none());
        assert_eq!(held_position, dragged_position);
        let outside = egui::pos2(end.x, response.rect.bottom() + 25.0);
        let (response, target, _) = seek_frame(
            &ctx,
            &mut preview,
            9.0,
            1,
            vec![
                egui::Event::PointerMoved(outside),
                pointer_button(outside, false),
            ],
        );
        assert_eq!(target, Some(Duration::from_secs_f64(dragged_position)));
        assert!(preview.is_none());
        assert!(
            seek_frame(&ctx, &mut preview, dragged_position, 1, vec![])
                .1
                .is_none()
        );

        // Keyboard adjustments remain available after a pointer gesture.
        ctx.memory_mut(|memory| memory.request_focus(response.id));
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                dragged_position,
                1,
                vec![key_press(egui::Key::ArrowRight)]
            )
            .1
            .is_some()
        );
    }

    #[test]
    fn seek_click_commits_on_release_and_cancelled_or_old_gestures_do_not_seek() {
        let ctx = egui::Context::default();
        crate::ui_style::configure_style(&ctx).expect("system fonts");
        let mut preview = None;
        let (response, _, _) = seek_frame(&ctx, &mut preview, 0.0, 1, vec![]);
        let point = response.rect.center();
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                1.0,
                1,
                vec![
                    egui::Event::PointerMoved(point),
                    pointer_button(point, true)
                ]
            )
            .1
            .is_none()
        );
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                2.0,
                1,
                vec![pointer_button(point, false)]
            )
            .1
            .is_some()
        );
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                3.0,
                1,
                vec![pointer_button(point, true)]
            )
            .1
            .is_none()
        );
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                4.0,
                1,
                vec![key_press(egui::Key::Escape)]
            )
            .1
            .is_none()
        );
        assert!(preview.is_none());
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                5.0,
                1,
                vec![pointer_button(point, false)]
            )
            .1
            .is_none()
        );

        seek_frame(
            &ctx,
            &mut preview,
            6.0,
            1,
            vec![pointer_button(point, true)],
        );
        // A new playback session clears the preview and owns a different widget ID.
        preview = None;
        assert!(seek_frame(&ctx, &mut preview, 0.0, 2, vec![]).1.is_none());
        assert!(
            seek_frame(
                &ctx,
                &mut preview,
                1.0,
                2,
                vec![pointer_button(point, false)]
            )
            .1
            .is_none()
        );
    }

    #[test]
    fn failed_load_is_read_only_and_preserves_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        std::fs::write(&path, "broken data").unwrap();
        let mut app = LoudTraceApp::load(ProjectStore::new(path.clone()));
        assert!(app.read_only);
        assert!(!app.commit(|l| l.projects.push(Project::new("Must not save".into()))));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken data");
    }

    #[test]
    fn concurrent_app_is_read_only_and_cannot_overwrite_first_app_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        let mut first = LoudTraceApp::load(ProjectStore::new(path.clone()));
        assert!(!first.read_only);
        assert!(first.commit(|library| library.projects.push(Project::new("First".into()))));

        let mut second = LoudTraceApp::load(ProjectStore::new(path.clone()));
        assert!(second.read_only);
        assert!(second.notice.as_ref().unwrap().contains("read-only"));
        assert_eq!(second.library.projects[0].name, "First");

        assert!(first.commit(|library| library.projects.push(Project::new("Later".into()))));
        assert!(!second.commit(|library| library.projects.clear()));
        let saved = ProjectStore::new(path.clone()).load().unwrap();
        assert_eq!(saved.projects.len(), 2);
        assert_eq!(saved.projects[1].name, "Later");

        drop(first);
        let reopened = LoudTraceApp::load(ProjectStore::new(path));
        assert!(!reopened.read_only);
        assert_eq!(reopened.library.projects.len(), 2);
        assert!(second.read_only);
    }

    #[test]
    fn replacing_file_clears_old_results_and_persists_new_reference() {
        let (dir, mut app) = test_app();
        let mut project = Project::new("Release".into());
        project.tracks.push(Track::new(dir.path().join("old.wav")));
        let id = project.id;
        let mut report = Report::default();
        report
            .tracks
            .insert(project.tracks[0].id, failed(&project.tracks[0], "old"));
        app.selected = Some(id);
        assert!(app.commit(|l| l.projects.push(project)));
        app.reports.insert(id, report);
        let new_path = dir.path().join("new.wav");
        app.edit_project(|p| p.tracks[0].path = new_path.clone());
        assert!(!app.reports.contains_key(&id));
        assert_eq!(
            app.store.load().unwrap().projects[0].tracks[0].path,
            new_path
        );
    }

    // One second of a stereo 1 kHz tone, about `dbfs` LUFS.
    fn write_tone(path: &std::path::Path, dbfs: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let amplitude = 10_f64.powf(dbfs / 20.0);
        for frame in 0..48_000 {
            let phase = std::f64::consts::TAU * 1000.0 * frame as f64 / 48_000.0;
            let sample = (amplitude * phase.sin()) as f32;
            writer.write_sample(sample).unwrap();
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    // Analyzes the track's file, then pins the reported values so expected
    // gains are exact. The file's real gating blocks still pool into albums.
    fn measured(
        track: &Track,
        integrated_lufs: f64,
        true_peak_dbtp: f64,
        duration_seconds: f64,
    ) -> TrackResult {
        let mut analysis = audio::analyze_track(&track.path, &AtomicBool::new(false)).unwrap();
        analysis.measurement = audio::Measurement {
            integrated_lufs: Some(integrated_lufs),
            true_peak_dbtp: Some(true_peak_dbtp),
            duration_seconds,
            sample_rate: 48_000,
            channels: 2,
        };
        TrackResult {
            path: track.path.clone(),
            stamp: file_stamp(&track.path).ok(),
            analysis: Ok(analysis),
        }
    }

    fn failed(track: &Track, error: &str) -> TrackResult {
        TrackResult {
            path: track.path.clone(),
            stamp: file_stamp(&track.path).ok(),
            analysis: Err(error.into()),
        }
    }

    // Tracks measure −9 and −20 LUFS at −2 dBTP (gains −5 and +1 dB) and share
    // a −11.5 LUFS album (gain −2.5 dB).
    fn audition_report(tracks: &[Track]) -> Report {
        let mut report = Report::default();
        for (track, loudness) in tracks.iter().zip([-9.0, -20.0]) {
            report
                .tracks
                .insert(track.id, measured(track, loudness, -2.0, 10.0));
        }
        report.album = Some(Ok(AlbumMeasurement {
            integrated_lufs: Some(-11.5),
            true_peak_dbtp: Some(-2.0),
        }));
        report
    }

    fn reorder_app() -> (tempfile::TempDir, LoudTraceApp) {
        let (dir, mut app) = test_app();
        let paths = [dir.path().join("first.wav"), dir.path().join("second.wav")];
        write_tone(&paths[0], -9.0);
        write_tone(&paths[1], -20.0);
        let mut project = Project::new("Release".into());
        project.tracks = paths.into_iter().map(Track::new).collect();
        let project_id = project.id;
        let track_id = project.tracks[0].id;
        let report = audition_report(&project.tracks);
        app.selected = Some(project_id);
        assert!(app.commit(|library| library.projects.push(project)));
        app.reports.insert(project_id, report);
        // Keep a playback session without opening an audio device in the test process.
        app.audition = Some(Audition {
            session_id: Uuid::new_v4(),
            project_id,
            track_id,
            finished: false,
        });
        app.seek_preview = Some(4.25);
        (dir, app)
    }

    fn four_track_reorder_app() -> (tempfile::TempDir, LoudTraceApp) {
        let (dir, mut app) = reorder_app();
        let mut tracks = Vec::new();
        for (name, loudness) in [("third.wav", -16.0), ("fourth.wav", -12.0)] {
            let path = dir.path().join(name);
            write_tone(&path, loudness);
            tracks.push(Track::new(path));
        }
        let report = app.reports.get_mut(&app.selected.unwrap()).unwrap();
        // Gains +2 and −2 dB.
        for (track, (loudness, duration)) in tracks.iter().zip([(-16.0, 30.0), (-12.0, 40.0)]) {
            report
                .tracks
                .insert(track.id, measured(track, loudness, -4.0, duration));
        }
        assert!(app.commit(|library| library.projects[0].tracks.extend(tracks)));
        (dir, app)
    }

    fn track_table_frame(
        ctx: &egui::Context,
        app: &LoudTraceApp,
        events: Vec<egui::Event>,
    ) -> Option<Action> {
        let mut action = None;
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 400.0),
                )),
                focused: true,
                events,
                ..Default::default()
            },
            |ui| {
                let project = app.project().unwrap();
                app.track_table(ui, project, app.reports.get(&project.id), &mut action);
            },
        )
        .drop_without_applying_deltas();
        action
    }

    #[test]
    fn full_track_table_drag_surfaces_produce_a_reorder_on_release() {
        for use_grip in [false, true] {
            let (_dir, mut app) = four_track_reorder_app();
            let tracks = app.project().unwrap().tracks.clone();
            let project_id = app.selected.unwrap();
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            track_table_frame(&ctx, &app, vec![]);
            track_table_frame(&ctx, &app, vec![]);
            let mut surfaces: Vec<_> = ctx
                .interactive_rects_last_pass()
                .into_iter()
                .filter(|rect| {
                    (rect.height() - ROW_HEIGHT).abs() < 0.1
                        && if use_grip {
                            rect.width() < 30.0
                        } else {
                            rect.width() > 400.0
                        }
                })
                .collect();
            surfaces.sort_by(|a, b| a.top().total_cmp(&b.top()));
            assert_eq!(
                surfaces.len(),
                4,
                "Unexpected drag surface rectangles: {surfaces:?}"
            );
            let start = egui::pos2(surfaces[1].left() + 12.0, surfaces[1].center().y);
            let target = egui::pos2(start.x, surfaces[3].top() + 4.0);
            assert!(
                track_table_frame(
                    &ctx,
                    &app,
                    vec![
                        egui::Event::PointerMoved(start),
                        pointer_button(start, true)
                    ],
                )
                .is_none()
            );
            assert!(
                track_table_frame(
                    &ctx,
                    &app,
                    vec![egui::Event::PointerMoved(start + Vec2::new(0.0, 10.0))],
                )
                .is_none()
            );
            assert!(
                track_drag::is_dragging(&ctx, project_id, tracks[1].id),
                "Drag did not start from {}: dragged widget {:?}",
                if use_grip { "grip" } else { "title" },
                ctx.dragged_id()
            );
            assert!(
                track_table_frame(&ctx, &app, vec![egui::Event::PointerMoved(target)]).is_none()
            );
            assert!(track_drag::is_dragging(&ctx, project_id, tracks[1].id));
            let action = track_table_frame(&ctx, &app, vec![pointer_button(target, false)]);
            let Some(Action::Reorder {
                project_id: dropped_project,
                track_id,
                insert_before,
            }) = action
            else {
                panic!("Releasing the drag inside the fourth row did not emit Reorder");
            };
            assert_eq!(dropped_project, project_id);
            assert_eq!(track_id, tracks[1].id);
            assert_eq!(insert_before, Some(tracks[3].id));
            app.reorder_track(dropped_project, track_id, insert_before);
            let order: Vec<_> = app
                .project()
                .unwrap()
                .tracks
                .iter()
                .map(|track| track.id)
                .collect();
            assert_eq!(
                order,
                [tracks[0].id, tracks[2].id, tracks[1].id, tracks[3].id]
            );
        }
    }

    fn full_app_frame(ctx: &egui::Context, app: &mut LoudTraceApp, events: Vec<egui::Event>) {
        let mut frame = eframe::Frame::_new_kittest();
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 600.0),
                )),
                focused: true,
                events,
                ..Default::default()
            },
            |ui| eframe::App::ui(app, ui, &mut frame),
        )
        .drop_without_applying_deltas();
    }

    #[test]
    fn full_app_panels_allow_title_and_grip_drag_with_scrollable_tracks() {
        for use_grip in [false, true] {
            let (dir, mut app) = four_track_reorder_app();
            let project_id = app.selected.unwrap();
            app.stop_playback();
            app.reports.clear();
            let additional: Vec<_> = (4..20)
                .map(|index| {
                    let path = dir.path().join(format!("track-{index}.wav"));
                    std::fs::write(&path, [0_u8; 16]).unwrap();
                    Track::new(path)
                })
                .collect();
            assert!(app.commit(|library| library.projects[0].tracks.extend(additional)));
            let tracks = app.project().unwrap().tracks.clone();
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");
            full_app_frame(&ctx, &mut app, vec![]);
            full_app_frame(&ctx, &mut app, vec![]);
            let mut surfaces: Vec<_> = ctx
                .interactive_rects_last_pass()
                .into_iter()
                .filter(|rect| {
                    (rect.height() - ROW_HEIGHT).abs() < 0.1
                        && if use_grip {
                            (rect.width() - 24.0).abs() < 0.1
                        } else {
                            rect.width() > 250.0
                        }
                })
                .collect();
            surfaces.sort_by(|a, b| a.top().total_cmp(&b.top()));
            assert!(
                surfaces.len() >= 5 && surfaces.len() < 20,
                "Expected a partially visible track list: {surfaces:?}"
            );
            let start = egui::pos2(surfaces[1].left() + 12.0, surfaces[1].center().y);
            let target = egui::pos2(start.x, surfaces[4].top() + 4.0);
            full_app_frame(
                &ctx,
                &mut app,
                vec![
                    egui::Event::PointerMoved(start),
                    pointer_button(start, true),
                ],
            );
            full_app_frame(
                &ctx,
                &mut app,
                vec![egui::Event::PointerMoved(start + Vec2::new(0.0, 10.0))],
            );
            assert!(
                track_drag::is_dragging(&ctx, project_id, tracks[1].id),
                "Full-app drag did not start from {}: dragged widget {:?}",
                if use_grip { "grip" } else { "title" },
                ctx.dragged_id()
            );
            full_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(target)]);
            assert!(track_drag::is_dragging(&ctx, project_id, tracks[1].id));
            full_app_frame(&ctx, &mut app, vec![pointer_button(target, false)]);
            let order: Vec<_> = app
                .project()
                .unwrap()
                .tracks
                .iter()
                .map(|track| track.id)
                .collect();
            let mut expected: Vec<_> = tracks.iter().map(|track| track.id).collect();
            expected.remove(1);
            expected.insert(3, tracks[1].id);
            assert_eq!(order, expected);
            assert!(egui::DragAndDrop::payload::<TrackDrag>(&ctx).is_none());
        }
    }

    #[test]
    fn identity_reordering_moves_across_tracks_and_preserves_analysis() {
        let (_dir, mut app) = four_track_reorder_app();
        let id = app.selected.unwrap();
        let tracks = app.project().unwrap().tracks.clone();
        let original_report = format!("{:?}", app.reports[&id]);
        let session = app.audition.as_ref().unwrap().session_id;
        let gains = [-5.0, 1.0, 2.0, -2.0];
        for (source, before, expected) in [
            (2, Some(0), [2, 0, 1, 3]),
            (0, Some(3), [2, 1, 0, 3]),
            (1, None, [2, 0, 3, 1]),
            (3, Some(2), [3, 2, 0, 1]),
        ] {
            app.reorder_track(id, tracks[source].id, before.map(|index| tracks[index].id));
            let saved = app.store.load().unwrap();
            let project = app.project().unwrap();
            let report = &app.reports[&id];
            for (index, original) in expected.into_iter().enumerate() {
                let track = &project.tracks[index];
                assert_eq!(track.id, tracks[original].id);
                assert_eq!(saved.projects[0].tracks[index].id, tracks[original].id);
                assert_eq!(saved.projects[0].tracks[index].path, tracks[original].path);
                assert_eq!(
                    audition_gain(report, AnalysisMode::Track, track),
                    Some(gains[original])
                );
                assert_eq!(
                    audition_gain(report, AnalysisMode::Album, track),
                    Some(-2.5)
                );
            }
            // Results are keyed by track, so reordering never touches them.
            assert_eq!(format!("{report:?}"), original_report);
            let audition = app.audition.as_ref().unwrap();
            assert_eq!(audition.session_id, session);
            assert_eq!(audition.project_id, id);
            assert_eq!(audition.track_id, tracks[0].id);
            assert!(!audition.finished);
            assert_eq!(app.seek_preview, Some(4.25));
            app.last_file_check = Instant::now() - Duration::from_secs(2);
            app.check_changed_files();
            assert!(app.reports.contains_key(&id));
            assert!(app.notice.is_none());
        }

        // Only the changed track loses its result, and with it the album.
        std::fs::write(&tracks[0].path, [4_u8; 96]).unwrap();
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();
        let report = &app.reports[&id];
        assert!(report.result(&tracks[0]).is_none());
        assert!(tracks[1..].iter().all(|t| report.measurement(t).is_some()));
        assert!(report.album.is_none());
        assert!(app.audition.is_none());
        assert!(app.seek_preview.is_none());
    }

    #[test]
    fn adjacent_menu_moves_keep_intervening_tracks_in_order() {
        let (_dir, mut app) = four_track_reorder_app();
        let tracks = app.project().unwrap().tracks.clone();
        for (source, delta, expected) in [
            (1, 1, [0, 2, 1, 3]),
            (3, -1, [0, 2, 3, 1]),
            (0, 1, [2, 0, 3, 1]),
            (1, -1, [2, 0, 1, 3]),
        ] {
            app.move_track(tracks[source].id, delta);
            let actual: Vec<_> = app
                .project()
                .unwrap()
                .tracks
                .iter()
                .map(|track| track.id)
                .collect();
            assert_eq!(actual, expected.map(|index| tracks[index].id));
        }
    }

    #[test]
    fn identity_reorder_rejects_stale_targets_and_noops_without_saving() {
        let (dir, mut app) = four_track_reorder_app();
        let id = app.selected.unwrap();
        let tracks = app.project().unwrap().tracks.clone();
        let original_library = serde_json::to_value(&app.library).unwrap();
        let original_report = format!("{:?}", app.reports[&id]);
        let session = app.audition.as_ref().unwrap().session_id;
        let blocked_parent = dir.path().join("not_a_directory");
        std::fs::write(&blocked_parent, b"block saving").unwrap();
        app.store = ProjectStore::new(blocked_parent.join("projects.json"));

        for (project_id, track_id, before) in [
            (id, tracks[1].id, Some(tracks[1].id)),
            (id, tracks[1].id, Some(tracks[2].id)),
            (id, tracks[3].id, None),
            (id, tracks[0].id, Some(Uuid::new_v4())),
            (id, Uuid::new_v4(), None),
            (Uuid::new_v4(), tracks[0].id, None),
        ] {
            app.reorder_track(project_id, track_id, before);
        }
        app.read_only = true;
        app.reorder_track(id, tracks[0].id, None);
        app.read_only = false;
        let (_tx, rx) = mpsc::channel();
        app.worker = Some(Worker {
            project_id: id,
            rx,
            cancelled: Arc::new(AtomicBool::new(false)),
            completed: 0,
            total: 4,
        });
        app.reorder_track(id, tracks[0].id, None);
        app.worker = None;
        app.selected = None;
        app.reorder_track(id, tracks[0].id, None);

        assert_eq!(
            serde_json::to_value(&app.library).unwrap(),
            original_library
        );
        assert_eq!(format!("{:?}", app.reports[&id]), original_report);
        assert_eq!(app.audition.as_ref().unwrap().session_id, session);
        assert_eq!(app.seek_preview, Some(4.25));
        assert!(
            app.notice.is_none(),
            "No-op and rejected drops must not attempt a save"
        );
    }

    #[test]
    fn failed_long_distance_drop_preserves_saved_and_live_state() {
        let (dir, mut app) = four_track_reorder_app();
        let id = app.selected.unwrap();
        let tracks = app.project().unwrap().tracks.clone();
        let original_library = serde_json::to_value(&app.library).unwrap();
        let original_report = format!("{:?}", app.reports[&id]);
        let session = app.audition.as_ref().unwrap().session_id;
        let saved_path = dir.path().join("projects.json");
        let original_saved = std::fs::read(&saved_path).unwrap();
        let blocked_parent = dir.path().join("not_a_directory");
        std::fs::write(&blocked_parent, b"block saving").unwrap();
        app.store = ProjectStore::new(blocked_parent.join("projects.json"));

        for (source, before) in [(0, None), (3, Some(tracks[0].id))] {
            app.reorder_track(id, tracks[source].id, before);
            assert_eq!(
                serde_json::to_value(&app.library).unwrap(),
                original_library
            );
            assert_eq!(format!("{:?}", app.reports[&id]), original_report);
            assert_eq!(std::fs::read(&saved_path).unwrap(), original_saved);
            let audition = app.audition.as_ref().unwrap();
            assert_eq!(audition.session_id, session);
            assert_eq!(audition.track_id, tracks[0].id);
            assert!(!audition.finished);
            assert_eq!(app.seek_preview, Some(4.25));
            assert!(
                app.notice
                    .as_ref()
                    .unwrap()
                    .contains("Changes were not saved")
            );
        }
    }

    #[test]
    fn reordering_preserves_track_gains_album_results_and_audition() {
        for mode in [AnalysisMode::Track, AnalysisMode::Album] {
            let (_dir, mut app) = reorder_app();
            assert!(app.commit(|library| library.projects[0].mode = mode));
            let project = app.project().unwrap();
            let id = project.id;
            let original_tracks = project.tracks.clone();
            let original_report = format!("{:?}", app.reports[&id]);
            let session = app.audition.as_ref().unwrap().session_id;
            let ctx = egui::Context::default();
            crate::ui_style::configure_style(&ctx).expect("system fonts");

            app.dispatch(Action::Move(original_tracks[0].id, 1), &ctx);

            let saved = app.store.load().unwrap();
            for (index, original) in original_tracks.iter().rev().enumerate() {
                assert_eq!(app.project().unwrap().tracks[index].id, original.id);
                assert_eq!(saved.projects[0].tracks[index].id, original.id);
                assert_eq!(saved.projects[0].tracks[index].path, original.path);
            }
            assert_eq!(saved.projects[0].mode, mode);
            let report = &app.reports[&id];
            assert_eq!(format!("{report:?}"), original_report);
            let tracks = &app.project().unwrap().tracks;
            assert_eq!(
                audition_gain(report, AnalysisMode::Track, &tracks[0]),
                Some(1.0)
            );
            assert_eq!(
                audition_gain(report, AnalysisMode::Track, &tracks[1]),
                Some(-5.0)
            );
            assert_eq!(
                audition_gain(report, AnalysisMode::Album, &tracks[0]),
                Some(-2.5)
            );
            assert_eq!(
                audition_gain(report, AnalysisMode::Album, &tracks[1]),
                Some(-2.5)
            );
            let album = report.album.as_ref().unwrap().as_ref().unwrap();
            assert_eq!(album.integrated_lufs, Some(-11.5));
            assert_eq!(album.true_peak_dbtp, Some(-2.0));
            let audition = app.audition.as_ref().unwrap();
            assert_eq!(audition.session_id, session);
            assert_eq!(audition.project_id, id);
            assert_eq!(audition.track_id, original_tracks[0].id);
            assert!(!audition.finished);
            assert_eq!(app.seek_preview, Some(4.25));

            app.dispatch(Action::Move(original_tracks[0].id, -1), &ctx);
            assert_eq!(app.project().unwrap().tracks[0].id, original_tracks[0].id);
            assert_eq!(
                audition_gain(&app.reports[&id], mode, &original_tracks[0]),
                Some(if mode == AnalysisMode::Track {
                    -5.0
                } else {
                    -2.5
                })
            );
            assert_eq!(app.audition.as_ref().unwrap().session_id, session);
            assert_eq!(app.seek_preview, Some(4.25));
        }
    }

    #[test]
    fn reordered_stamps_survive_file_checks_and_still_detect_later_edits() {
        let (_dir, mut app) = reorder_app();
        let id = app.selected.unwrap();
        let [track, other] = app.project().unwrap().tracks.clone().try_into().unwrap();
        let session = app.audition.as_ref().unwrap().session_id;
        app.move_track(track.id, 1);
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();
        assert!(app.reports.contains_key(&id));
        assert_eq!(app.audition.as_ref().unwrap().session_id, session);
        assert_eq!(app.seek_preview, Some(4.25));
        assert!(app.notice.is_none());

        std::fs::write(&track.path, [2_u8; 48]).unwrap();
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();
        assert!(app.reports[&id].result(&track).is_none());
        assert!(app.reports[&id].measurement(&other).is_some());
        assert!(app.audition.is_none());
        assert!(app.seek_preview.is_none());
        assert_eq!(app.playback.state(), PlaybackState::Stopped);
        assert!(app.notice.as_ref().unwrap().contains("Analyze again"));
    }

    #[test]
    fn failed_reorder_save_leaves_order_results_and_playback_session_intact() {
        let (dir, mut app) = reorder_app();
        let id = app.selected.unwrap();
        let track_id = app.project().unwrap().tracks[0].id;
        let original_library = serde_json::to_value(&app.library).unwrap();
        let original_report = format!("{:?}", app.reports[&id]);
        let session = app.audition.as_ref().unwrap().session_id;
        let saved_path = dir.path().join("projects.json");
        let original_saved = std::fs::read(&saved_path).unwrap();
        let blocked_parent = dir.path().join("not_a_directory");
        std::fs::write(&blocked_parent, b"block saving").unwrap();
        app.store = ProjectStore::new(blocked_parent.join("projects.json"));

        app.move_track(track_id, 1);

        assert_eq!(
            serde_json::to_value(&app.library).unwrap(),
            original_library
        );
        assert_eq!(format!("{:?}", app.reports[&id]), original_report);
        assert_eq!(std::fs::read(saved_path).unwrap(), original_saved);
        let audition = app.audition.as_ref().unwrap();
        assert_eq!(audition.session_id, session);
        assert_eq!(audition.project_id, id);
        assert_eq!(audition.track_id, track_id);
        assert!(!audition.finished);
        assert_eq!(app.seek_preview, Some(4.25));
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .contains("Changes were not saved")
        );
    }

    #[test]
    fn invalid_or_disabled_reorders_leave_results_and_session_untouched() {
        let (_dir, mut app) = reorder_app();
        let id = app.selected.unwrap();
        let tracks = app.project().unwrap().tracks.clone();
        let original_library = serde_json::to_value(&app.library).unwrap();
        let original_report = format!("{:?}", app.reports[&id]);
        let session = app.audition.as_ref().unwrap().session_id;
        for (track_id, delta) in [
            (tracks[0].id, -1),
            (tracks[1].id, 1),
            (tracks[0].id, 0),
            (tracks[0].id, isize::MIN),
            (tracks[1].id, isize::MAX),
            (Uuid::new_v4(), 1),
        ] {
            app.move_track(track_id, delta);
        }
        app.read_only = true;
        app.move_track(tracks[0].id, 1);
        app.read_only = false;
        let (_tx, rx) = mpsc::channel();
        app.worker = Some(Worker {
            project_id: id,
            rx,
            cancelled: Arc::new(AtomicBool::new(false)),
            completed: 0,
            total: 2,
        });
        app.move_track(tracks[0].id, 1);
        app.worker = None;
        app.selected = None;
        app.move_track(tracks[0].id, 1);

        assert_eq!(
            serde_json::to_value(&app.library).unwrap(),
            original_library
        );
        assert_eq!(
            serde_json::to_value(app.store.load().unwrap()).unwrap(),
            original_library
        );
        assert_eq!(format!("{:?}", app.reports[&id]), original_report);
        assert_eq!(app.audition.as_ref().unwrap().session_id, session);
        assert_eq!(app.seek_preview, Some(4.25));
        assert!(app.notice.is_none());
    }

    #[test]
    fn reordering_keeps_track_errors_and_missing_stamps_with_their_sources() {
        let (_dir, mut app) = reorder_app();
        let id = app.selected.unwrap();
        let [track, other] = app.project().unwrap().tracks.clone().try_into().unwrap();
        std::fs::remove_file(&track.path).unwrap();
        let report = app.reports.get_mut(&id).unwrap();
        report
            .tracks
            .insert(track.id, failed(&track, "Missing source"));
        report.sync(&app.library.projects[0]);
        app.stop_playback();

        app.move_track(track.id, 1);
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();

        let report = &app.reports[&id];
        assert_eq!(app.project().unwrap().tracks[1].id, track.id);
        assert!(report.tracks[&track.id].stamp.is_none());
        assert!(report.tracks[&other.id].stamp.is_some());
        assert_eq!(
            report.result(&track).unwrap().as_ref().unwrap_err(),
            "Missing source"
        );
        assert!(report.album.as_ref().unwrap().is_err());
        assert_eq!(
            audition_gain(report, AnalysisMode::Track, &other),
            Some(1.0)
        );
        assert_eq!(audition_gain(report, AnalysisMode::Track, &track), None);
        assert_eq!(audition_gain(report, AnalysisMode::Album, &other), None);
        assert!(app.notice.is_none());
    }

    #[test]
    fn audition_uses_track_peak_limit_or_shared_album_gain() {
        let (_dir, mut app) = reorder_app();
        let id = app.selected.unwrap();
        let [first, second] = app.project().unwrap().tracks.clone().try_into().unwrap();
        let report = app.reports.get_mut(&id).unwrap();
        assert_eq!(
            audition_gain(report, AnalysisMode::Track, &first),
            Some(-5.0)
        );
        assert_eq!(
            audition_gain(report, AnalysisMode::Track, &second),
            Some(1.0)
        );
        assert_eq!(
            audition_gain(report, AnalysisMode::Album, &first),
            Some(-2.5)
        );
        assert_eq!(
            audition_gain(report, AnalysisMode::Album, &second),
            Some(-2.5)
        );
        report
            .tracks
            .insert(second.id, failed(&second, "Missing file"));
        assert_eq!(audition_gain(report, AnalysisMode::Album, &second), None);
        report.album = Some(Err("Incomplete album".into()));
        assert_eq!(audition_gain(report, AnalysisMode::Album, &first), None);
        assert_eq!(
            audition_gain(report, AnalysisMode::Track, &first),
            Some(-5.0)
        );
        // A result never describes a track that now points at another file.
        let relinked = Track {
            path: first.path.with_file_name("other.wav"),
            ..first.clone()
        };
        assert_eq!(audition_gain(report, AnalysisMode::Track, &relinked), None);
    }

    #[test]
    fn playback_revalidates_files_without_waiting_for_periodic_check() {
        let (dir, mut app) = test_app();
        let path = dir.path().join("song.wav");
        write_tone(&path, -9.0);
        let mut project = Project::new("Release".into());
        let track = Track::new(path.clone());
        let track_id = track.id;
        project.tracks.push(track);
        let id = project.id;
        let report = audition_report(&project.tracks);
        app.selected = Some(id);
        app.commit(|l| l.projects.push(project));
        app.reports.insert(id, report);
        std::fs::write(&path, [1_u8; 32]).unwrap();
        app.start_playback(track_id);
        assert_eq!(app.playback.state(), PlaybackState::Stopped);
        assert!(app.audition.is_none());
        assert!(!app.reports.contains_key(&id));
        assert!(app.notice.as_ref().unwrap().contains("Analyze again"));
    }

    #[test]
    fn another_tracks_file_changing_keeps_individual_playback_but_not_album_playback() {
        let (_dir, mut app) = reorder_app();
        let id = app.selected.unwrap();
        let [first, second] = app.project().unwrap().tracks.clone().try_into().unwrap();
        let session = app.audition.as_ref().unwrap().session_id;
        std::fs::write(&second.path, [1_u8; 32]).unwrap();
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();
        assert_eq!(app.audition.as_ref().unwrap().session_id, session);
        assert!(app.reports[&id].measurement(&first).is_some());
        assert!(app.reports[&id].result(&second).is_none());
        assert!(app.notice.as_ref().unwrap().contains("Analyze again"));

        // The shared album gain needs every track.
        let ctx = egui::Context::default();
        app.dispatch(Action::Mode(AnalysisMode::Album), &ctx);
        assert!(app.audition.is_none());
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .contains("unavailable in this mode")
        );
    }

    #[test]
    fn editing_one_track_keeps_the_others_and_individual_playback() {
        for mode in [AnalysisMode::Track, AnalysisMode::Album] {
            let (dir, mut app) = reorder_app();
            assert!(app.commit(|library| library.projects[0].mode = mode));
            let id = app.selected.unwrap();
            let session = app.audition.as_ref().unwrap().session_id;
            let added = dir.path().join("added.wav");
            write_tone(&added, -14.0);
            app.add_paths(vec![added]);

            let tracks = app.project().unwrap().tracks.clone();
            let report = &app.reports[&id];
            assert!(report.measurement(&tracks[0]).is_some());
            assert!(report.measurement(&tracks[1]).is_some());
            assert!(report.result(&tracks[2]).is_none());
            assert!(report.album.is_none());
            let pending: Vec<_> = unmeasured(Some(report), app.project().unwrap())
                .map(|t| t.id)
                .collect();
            assert_eq!(pending, [tracks[2].id]);
            assert_eq!(
                app.audition.as_ref().map(|a| a.session_id),
                (mode == AnalysisMode::Track).then_some(session)
            );

            if mode == AnalysisMode::Track {
                let ctx = egui::Context::default();
                app.dispatch(Action::Remove(tracks[0].id), &ctx);
                assert!(app.audition.is_none());
                assert!(!app.reports[&id].tracks.contains_key(&tracks[0].id));
                assert!(app.reports[&id].measurement(&tracks[1]).is_some());
            }
        }
    }

    fn analyze(app: &mut LoudTraceApp, ctx: &egui::Context) -> usize {
        app.start_analysis(ctx);
        let total = app.worker.as_ref().unwrap().total;
        let deadline = Instant::now() + Duration::from_secs(30);
        while app.worker.is_some() {
            assert!(Instant::now() < deadline, "Analysis did not finish");
            std::thread::sleep(Duration::from_millis(5));
            app.poll_worker();
        }
        total
    }

    // The album pooled from cached tracks must equal analyzing every file at once.
    fn assert_album_matches_batch(app: &LoudTraceApp, paths: &[PathBuf]) {
        let expected = audio::analyze_files(paths, |_, _| {}, &AtomicBool::new(false))
            .album
            .unwrap();
        let report = &app.reports[&app.selected.unwrap()];
        let actual = report.album.as_ref().unwrap().as_ref().unwrap();
        let difference = actual.integrated_lufs.unwrap() - expected.integrated_lufs.unwrap();
        assert!(
            difference.abs() < 1e-9,
            "Pooled album differs by {difference} LU"
        );
        assert_eq!(actual.true_peak_dbtp, expected.true_peak_dbtp);
    }

    #[test]
    fn analysis_measures_only_new_or_changed_tracks_and_pools_the_album() {
        let (dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let mut paths: Vec<_> = [-18.0, -26.0, -12.0]
            .into_iter()
            .enumerate()
            .map(|(index, loudness)| {
                let path = dir.path().join(format!("track-{index}.wav"));
                write_tone(&path, loudness);
                path
            })
            .collect();
        let mut project = Project::new("Release".into());
        project.mode = AnalysisMode::Album;
        project.tracks = paths.iter().cloned().map(Track::new).collect();
        let id = project.id;
        app.selected = Some(id);
        assert!(app.commit(|library| library.projects.push(project)));

        assert_eq!(analyze(&mut app, &ctx), 3);
        assert_album_matches_batch(&app, &paths);

        // Relinking one track measures only that track, then pools the album again.
        paths[1] = dir.path().join("replacement.wav");
        write_tone(&paths[1], -30.0);
        app.edit_project(|p| p.tracks[1].path = paths[1].clone());
        assert!(app.reports[&id].album.is_none());
        assert_eq!(analyze(&mut app, &ctx), 1);
        assert_album_matches_batch(&app, &paths);

        // Re-exporting a file on disk measures only that file again.
        write_tone(&paths[0], -20.0);
        // Some filesystems store coarse modification times.
        std::fs::File::options()
            .write(true)
            .open(&paths[0])
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();
        let first = app.project().unwrap().tracks[0].clone();
        assert!(app.reports[&id].result(&first).is_none());
        assert_eq!(analyze(&mut app, &ctx), 1);
        assert_album_matches_batch(&app, &paths);

        // Removing a track needs no analysis at all.
        app.edit_project(|p| {
            p.tracks.pop();
        });
        paths.pop();
        assert!(app.worker.is_none());
        assert_album_matches_batch(&app, &paths);

        // With every track current, Analyze measures all of them again.
        assert_eq!(analyze(&mut app, &ctx), 2);
        assert_album_matches_batch(&app, &paths);
    }

    #[test]
    fn cancelled_analysis_keeps_tracks_that_finished() {
        let (_dir, mut app) = reorder_app();
        app.stop_playback();
        let id = app.selected.unwrap();
        let [first, second] = app.project().unwrap().tracks.clone().try_into().unwrap();
        let mut results = app.reports.remove(&id).unwrap().tracks;
        let (tx, rx) = mpsc::channel();
        app.worker = Some(Worker {
            project_id: id,
            rx,
            cancelled: Arc::new(AtomicBool::new(true)),
            completed: 0,
            total: 2,
        });
        // A result for a file the track no longer points at is never attached.
        let mut stale = results.remove(&first.id).unwrap();
        stale.path = first.path.with_file_name("old.wav");
        tx.send(WorkerEvent::Track(first.id, Box::new(stale)))
            .unwrap();
        tx.send(WorkerEvent::Track(
            second.id,
            Box::new(results.remove(&second.id).unwrap()),
        ))
        .unwrap();
        tx.send(WorkerEvent::Finished).unwrap();
        app.poll_worker();

        assert!(app.worker.is_none());
        let report = &app.reports[&id];
        assert!(report.measurement(&second).is_some());
        assert!(!report.tracks.contains_key(&first.id));
        assert!(report.album.is_none());
        assert!(app.notice.as_ref().unwrap().contains("keep their results"));
    }

    #[test]
    fn monitor_controls_leave_normalization_and_projects_unchanged() {
        let (_dir, mut app) = reorder_app();
        app.stop_playback();
        let id = app.selected.unwrap();
        let track = app.project().unwrap().tracks[0].clone();
        let ctx = egui::Context::default();
        crate::ui_style::configure_style(&ctx).expect("system fonts");
        app.dispatch(Action::Volume(0.7), &ctx);
        app.dispatch(Action::Mute, &ctx);
        assert!(app.muted);
        assert_eq!(app.monitor_volume, 0.7);
        assert_eq!(
            audition_gain(&app.reports[&id], AnalysisMode::Track, &track),
            Some(-5.0)
        );
        app.dispatch(Action::Mode(AnalysisMode::Album), &ctx);
        assert_eq!(app.monitor_volume, 0.7);
        assert_eq!(
            app.store.load().unwrap().projects[0].mode,
            AnalysisMode::Album
        );
        app.dispatch(Action::Mute, &ctx);
        assert!(!app.muted);
        assert_eq!(app.monitor_volume, 0.7);
    }

    #[test]
    fn overwriting_same_path_invalidates_report() {
        let (dir, mut app) = test_app();
        let path = dir.path().join("song.wav");
        std::fs::write(&path, [0_u8; 16]).unwrap();
        let mut project = Project::new("Release".into());
        project.tracks.push(Track::new(path.clone()));
        let id = project.id;
        let mut report = Report::default();
        report
            .tracks
            .insert(project.tracks[0].id, failed(&project.tracks[0], "old"));
        app.selected = Some(id);
        app.commit(|l| l.projects.push(project));
        app.reports.insert(id, report);
        app.audition = Some(Audition {
            session_id: Uuid::new_v4(),
            project_id: id,
            track_id: app.library.projects[0].tracks[0].id,
            finished: false,
        });
        std::fs::write(&path, [1_u8; 32]).unwrap();
        app.last_file_check = Instant::now() - Duration::from_secs(2);
        app.check_changed_files();
        assert!(!app.reports.contains_key(&id));
        assert!(app.audition.is_none());
        assert_eq!(app.playback.state(), PlaybackState::Stopped);
    }
}
