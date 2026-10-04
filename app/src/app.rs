//! Application state, transport, keyboard shortcuts, preview and render panels.
//! The timeline widget lives in `timeline.rs`.

use crate::audio::{AClip, ATrack, AudioEngine};
use crate::media::{self, MediaInfo, RgbaFrame};
use crate::model::{self, ClipId, Project, RenderSettings, Timeline, fmt_time};
use crate::render::{self, Phase, RenderJob};
use crate::video::{VClip, VFrame, VideoEngine};
use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{Color32, RichText, Ui};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "wmv", "flv", "m4v", "ts", "mts", "m2ts", "mp3", "wav", "flac", "ogg", "opus", "m4a"];

pub enum JobMsg {
    Probed { path: PathBuf, info: anyhow::Result<MediaInfo>, at: Option<f64> },
    Waveform { media: usize, stream: usize, peaks: Vec<f32> },
    Thumb { media: usize, t: f64, frame: RgbaFrame },
}

pub struct TimelineView {
    pub px_per_sec: f32,
    /// Timeline time at the left edge of the lanes.
    pub scroll: f64,
    pub fit_pending: bool,
    /// Scroll so the cursor is visible on the next frame (set by keyboard jumps).
    pub reveal: bool,
    /// Active scroll-bar drag.
    pub bar_drag: Option<BarDrag>,
}

#[derive(Clone, Copy)]
pub enum BarPart {
    Thumb,
    LeftEdge,
    RightEdge,
}

#[derive(Clone, Copy)]
pub struct BarDrag {
    pub part: BarPart,
    pub press_x: f32,
    pub scroll: f64,
    pub visible: f64,
    /// Seconds per scroll-bar pixel, frozen at drag start so the thumb tracks the mouse.
    pub secs_per_px: f64,
}

#[derive(Clone)]
pub enum DragKind {
    Move { ids: HashSet<ClipId>, audio_lane: bool },
    TrimLeft { ids: HashSet<ClipId> },
    TrimRight { ids: HashSet<ClipId> },
    Scrub,
    RegionNew { anchor: f64, prev: Option<(f64, f64)> },
    RegionStart,
    RegionEnd,
    RegionMove { orig: (f64, f64) },
    Pan { scroll: f64 },
}

#[derive(Clone)]
pub struct Drag {
    pub kind: DragKind,
    pub origin_t: f64,
    pub origin_y: f32,
    pub orig: Timeline,
    pub changed: bool,
}

pub struct App {
    pub project: Project,
    pub media: Vec<MediaInfo>,
    pub paths: Vec<Arc<PathBuf>>,
    pub waveforms: HashMap<(usize, usize), Arc<Vec<f32>>>,
    pub thumbs: HashMap<usize, Vec<(f64, egui::TextureHandle)>>,
    jobs_tx: Sender<JobMsg>,
    jobs_rx: Receiver<JobMsg>,
    pending_probes: usize,

    undo: Vec<Timeline>,
    redo: Vec<Timeline>,
    pub selection: HashSet<ClipId>,
    pub cursor: f64,

    pub playing: bool,
    play_origin: f64,
    play_start_t: f64,
    play_instant: Instant,
    pub loop_play: bool,
    video: VideoEngine,
    audio: AudioEngine,
    pending_frame: Option<VFrame>,
    preview_tex: Option<egui::TextureHandle>,
    preview_has_image: bool,

    pub view: TimelineView,
    pub drag: Option<Drag>,
    pub snapping: bool,
    pub ignore_grouping: bool,

    render_job: Option<RenderJob>,
    render_error: Option<String>,
    saved_settings: RenderSettings,

    project_path: Option<PathBuf>,
    pub status: String,
    ctx: egui::Context,
}

fn settings_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("WebmStudio").join("settings.json"))
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let (jobs_tx, jobs_rx) = unbounded();
        let c2 = ctx.clone();
        let video = VideoEngine::new(move || c2.request_repaint());
        let audio = AudioEngine::new();
        let settings: RenderSettings = settings_file()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut app = Self {
            project: Project { settings: settings.clone(), ..Default::default() },
            media: Vec::new(),
            paths: Vec::new(),
            waveforms: HashMap::new(),
            thumbs: HashMap::new(),
            jobs_tx,
            jobs_rx,
            pending_probes: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            selection: HashSet::new(),
            cursor: 0.0,
            playing: false,
            play_origin: 0.0,
            play_start_t: 0.0,
            play_instant: Instant::now(),
            loop_play: false,
            video,
            audio,
            pending_frame: None,
            preview_tex: None,
            preview_has_image: false,
            view: TimelineView { px_per_sec: 40.0, scroll: 0.0, fit_pending: false, reveal: false, bar_drag: None },
            drag: None,
            snapping: true,
            ignore_grouping: false,
            render_job: None,
            render_error: None,
            saved_settings: settings,
            project_path: None,
            status: "Drop video files here, or File ▸ Import (Ctrl+I)".into(),
            ctx,
        };
        // Files passed on the command line (e.g. "Open with" / drag onto the exe).
        for a in std::env::args().skip(1) {
            let p = PathBuf::from(a);
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wsproj")) {
                app.open_project(&p);
            } else if p.is_file() {
                app.import(p, None);
            }
        }
        app
    }

    pub fn fps(&self) -> f64 {
        self.project.settings.fps.max(1) as f64
    }

    pub fn frame_snap(&self, t: f64) -> f64 {
        (t * self.fps()).round() / self.fps()
    }

    // ───────────── media / jobs ─────────────

    pub fn import(&mut self, path: PathBuf, at: Option<f64>) {
        self.pending_probes += 1;
        let tx = self.jobs_tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let info = media::probe(&path);
            let _ = tx.send(JobMsg::Probed { path, info, at });
            ctx.request_repaint();
        });
    }

    fn add_media_info(&mut self, info: MediaInfo) -> usize {
        let idx = self.media.len();
        self.project.media.push(info.path.clone());
        self.paths.push(Arc::new(info.path.clone()));
        let path = info.path.clone();
        for (si, a) in info.audio.iter().enumerate() {
            let (tx, ctx, p, index) = (self.jobs_tx.clone(), self.ctx.clone(), path.clone(), a.index);
            std::thread::spawn(move || {
                if let Ok(peaks) = media::build_waveform(&p, index) {
                    let _ = tx.send(JobMsg::Waveform { media: idx, stream: si, peaks });
                    ctx.request_repaint();
                }
            });
        }
        if info.video.is_some() {
            let (tx, ctx, p, dur) = (self.jobs_tx.clone(), self.ctx.clone(), path, info.duration);
            std::thread::spawn(move || {
                let _ = media::build_thumbs(&p, dur, |t, frame| {
                    let _ = tx.send(JobMsg::Thumb { media: idx, t, frame });
                    ctx.request_repaint();
                });
            });
        }
        self.media.push(info);
        idx
    }

    fn poll_jobs(&mut self) {
        while let Ok(msg) = self.jobs_rx.try_recv() {
            match msg {
                JobMsg::Probed { path, info, at } => {
                    self.pending_probes = self.pending_probes.saturating_sub(1);
                    match info {
                        Ok(info) => {
                            self.checkpoint();
                            let at = at.unwrap_or_else(|| self.project.timeline.end());
                            let at = self.frame_snap(at.max(0.0));
                            let name = info.name();
                            let streams = info.audio.len();
                            let first = self.media.is_empty();
                            let idx = self.add_media_info(info);
                            let info = self.media[idx].clone();
                            let ids = self.project.timeline.add_media(idx, &info, at);
                            self.selection = ids.into_iter().collect();
                            self.status = format!("Imported {name} ({} audio track{})", streams, if streams == 1 { "" } else { "s" });
                            if first {
                                self.view.fit_pending = true;
                            }
                            self.timeline_changed();
                        }
                        Err(e) => self.status = format!("Cannot import {}: {e:#}", path.display()),
                    }
                }
                JobMsg::Waveform { media, stream, peaks } => {
                    self.waveforms.insert((media, stream), Arc::new(peaks));
                }
                JobMsg::Thumb { media, t, frame } => {
                    let img = egui::ColorImage::from_rgba_unmultiplied([frame.w, frame.h], &frame.pixels);
                    let tex = self.ctx.load_texture(format!("thumb{media}_{t}"), img, egui::TextureOptions::LINEAR);
                    let v = self.thumbs.entry(media).or_default();
                    v.push((t, tex));
                    v.sort_by(|a, b| a.0.total_cmp(&b.0));
                }
            }
        }
    }

    // ───────────── editing ─────────────

    pub fn checkpoint(&mut self) {
        self.undo.push(self.project.timeline.clone());
        if self.undo.len() > 300 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn push_undo(&mut self, tl: Timeline) {
        self.undo.push(tl);
        self.redo.clear();
    }

    // The region is not an edit: undo/redo never move or remove it.
    fn undo(&mut self) {
        if let Some(mut t) = self.undo.pop() {
            t.region = self.project.timeline.region;
            self.redo.push(std::mem::replace(&mut self.project.timeline, t));
            self.timeline_changed();
        }
    }

    fn redo(&mut self) {
        if let Some(mut t) = self.redo.pop() {
            t.region = self.project.timeline.region;
            self.undo.push(std::mem::replace(&mut self.project.timeline, t));
            self.timeline_changed();
        }
    }

    /// Selection expanded to linked clips (unless grouping is ignored).
    pub fn grouped(&self, ids: &HashSet<ClipId>) -> HashSet<ClipId> {
        if self.ignore_grouping { ids.clone() } else { self.project.timeline.expand_groups(ids) }
    }

    fn split(&mut self) {
        let t = self.cursor;
        let only = if self.selection.is_empty() { None } else { Some(self.grouped(&self.selection)) };
        let before = self.project.timeline.clone();
        if self.project.timeline.split_at(t, only.as_ref()) {
            self.push_undo(before);
            // Vegas selects the right-hand parts after a split.
            self.selection = self.project.timeline.all_clips().filter(|(_, c)| (c.start - t).abs() < 1e-6).map(|(_, c)| c.id).collect();
            self.timeline_changed();
        }
    }

    fn delete_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.checkpoint();
        let ids = self.grouped(&self.selection);
        self.project.timeline.delete(&ids);
        self.selection.clear();
        self.timeline_changed();
    }

    /// Re-send timeline snapshots to the preview engines.
    pub fn timeline_changed(&mut self) {
        let tl = &self.project.timeline;
        let v: Vec<VClip> = tl
            .video
            .iter()
            .map(|c| VClip { path: self.paths[c.media].clone(), start: c.start, src_in: c.src_in, src_out: c.src_out })
            .collect();
        self.video.set_timeline(v);
        self.audio_changed();
        if !self.playing {
            self.video.seek(self.cursor);
        }
    }

    /// Show the live timeline's frame at `t` without moving the cursor (used while trimming).
    pub fn preview_at(&mut self, t: f64) {
        let v: Vec<VClip> = self
            .project
            .timeline
            .video
            .iter()
            .map(|c| VClip { path: self.paths[c.media].clone(), start: c.start, src_in: c.src_in, src_out: c.src_out })
            .collect();
        self.video.set_timeline(v);
        self.video.seek(t);
    }

    pub fn audio_changed(&mut self) {
        let tl = &self.project.timeline;
        let gains = render::track_gains(&self.project);
        let a: Vec<ATrack> = tl
            .audio
            .iter()
            .enumerate()
            .map(|(i, t)| ATrack {
                gain: gains[i],
                clips: t
                    .clips
                    .iter()
                    .filter_map(|c| {
                        let s = self.media[c.media].audio.get(c.stream)?;
                        Some(AClip { id: c.id, path: self.paths[c.media].clone(), stream_index: s.index, start: c.start, src_in: c.src_in, src_out: c.src_out })
                    })
                    .collect(),
            })
            .collect();
        self.audio.set_tracks(a);
    }

    // ───────────── transport ─────────────

    pub fn now(&self) -> f64 {
        if !self.playing {
            return self.cursor;
        }
        if self.audio.has_device() {
            self.play_start_t + self.audio.played_secs()
        } else {
            self.play_start_t + self.play_instant.elapsed().as_secs_f64()
        }
    }

    fn start_playback(&mut self, from: f64) {
        self.play_start_t = from;
        self.play_instant = Instant::now();
        self.pending_frame = None;
        self.audio.play(from);
        self.video.play(from);
        self.playing = true;
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        let end = self.project.timeline.end();
        if self.loop_play && let Some((a, b)) = self.project.timeline.region {
            if self.cursor < a || self.cursor >= b {
                self.cursor = a;
            }
        } else if self.cursor >= end - 1e-3 {
            self.cursor = 0.0;
        }
        self.play_origin = self.cursor;
        self.start_playback(self.cursor);
    }

    /// Vegas "Stop": cursor returns to where playback started.
    pub fn stop(&mut self) {
        if !self.playing {
            return;
        }
        self.playing = false;
        self.audio.stop();
        self.video.stop();
        self.cursor = self.play_origin;
        self.video.seek(self.cursor);
    }

    /// Pause: cursor stays where playback is.
    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        let t = self.frame_snap(self.now());
        self.playing = false;
        self.audio.stop();
        self.video.stop();
        self.cursor = t;
        self.video.seek(t);
    }

    pub fn seek(&mut self, t: f64) {
        let t = t.max(0.0);
        self.cursor = t;
        if self.playing {
            self.play_origin = t;
            self.start_playback(t);
        } else {
            self.video.seek(t);
        }
    }

    /// Seek while dragging: fast keyframe preview, refined when the mouse rests.
    pub fn scrub(&mut self, t: f64) {
        let t = t.max(0.0);
        if self.playing {
            self.seek(t);
        } else {
            self.cursor = t;
            self.video.scrub(t);
        }
    }

    fn update_transport(&mut self) {
        if self.playing {
            let t = self.now();
            let end = self.project.timeline.end();
            if self.loop_play && let Some((a, b)) = self.project.timeline.region {
                if t >= b {
                    self.start_playback(a);
                }
            } else if t >= end {
                self.pause();
                self.cursor = end;
            }
            if self.playing {
                self.cursor = self.now();
            }
            self.ctx.request_repaint();
        }
        // Pull decoded frames; show the newest one that is due.
        let generation = self.video.generation();
        let mut show: Option<VFrame> = None;
        loop {
            let f = match self.pending_frame.take() {
                Some(f) => f,
                None => match self.video.frames.try_recv() {
                    Ok(f) => f,
                    Err(_) => break,
                },
            };
            if f.generation != generation {
                continue;
            }
            if self.playing && f.t > self.cursor + 0.004 {
                self.pending_frame = Some(f);
                break;
            }
            show = Some(f);
        }
        if let Some(f) = show {
            match f.image {
                Some(img) => {
                    let ci = egui::ColorImage::from_rgba_unmultiplied([img.w, img.h], &img.pixels);
                    match &mut self.preview_tex {
                        Some(t) => t.set(ci, egui::TextureOptions::LINEAR),
                        None => self.preview_tex = Some(self.ctx.load_texture("preview", ci, egui::TextureOptions::LINEAR)),
                    }
                    self.preview_has_image = true;
                }
                None => self.preview_has_image = false,
            }
        }
    }

    // ───────────── project files ─────────────

    fn new_project(&mut self) {
        if self.playing {
            self.stop();
        }
        let settings = self.project.settings.clone();
        self.project = Project { settings, ..Default::default() };
        self.media.clear();
        self.paths.clear();
        self.waveforms.clear();
        self.thumbs.clear();
        self.undo.clear();
        self.redo.clear();
        self.selection.clear();
        self.cursor = 0.0;
        self.project_path = None;
        self.timeline_changed();
    }

    fn save_project(&mut self, save_as: bool) {
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new().add_filter("WebM Studio project", &["wsproj"]).set_file_name("project.wsproj").save_file(),
        };
        let Some(path) = path else { return };
        match serde_json::to_string_pretty(&self.project).map_err(anyhow::Error::from).and_then(|s| Ok(std::fs::write(&path, s)?)) {
            Ok(()) => {
                self.status = format!("Saved {}", path.display());
                self.project_path = Some(path);
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
    }

    fn open_project_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("WebM Studio project", &["wsproj"]).pick_file() {
            self.open_project(&p);
        }
    }

    fn open_project(&mut self, path: &Path) {
        let loaded: anyhow::Result<Project> = std::fs::read_to_string(path).map_err(Into::into).and_then(|s| Ok(serde_json::from_str(&s)?));
        let proj = match loaded {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Cannot open project: {e}");
                return;
            }
        };
        let mut infos = Vec::new();
        for m in &proj.media {
            match media::probe(m) {
                Ok(i) => infos.push(i),
                Err(e) => {
                    self.status = format!("Missing media {}: {e:#}", m.display());
                    return;
                }
            }
        }
        self.new_project();
        let Project { timeline, settings, output, .. } = proj;
        for i in infos {
            self.add_media_info(i);
        }
        self.project.timeline = timeline;
        self.project.settings = settings;
        self.project.output = output;
        self.project_path = Some(path.to_path_buf());
        self.view.fit_pending = true;
        self.status = format!("Opened {}", path.display());
        self.timeline_changed();
    }

    fn import_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new().add_filter("Media", VIDEO_EXTS).add_filter("All files", &["*"]).pick_files() {
            for f in files {
                self.import(f, None);
            }
        }
    }

    // ───────────── input ─────────────

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        use egui::{Key, Modifiers};
        let before = self.cursor;
        let pressed =|m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        let (none, ctrl, shift) = (Modifiers::NONE, Modifiers::COMMAND, Modifiers::SHIFT);
        let cs = Modifiers::COMMAND | Modifiers::SHIFT;

        if pressed(cs, Key::S) {
            self.save_project(true);
        } else if pressed(ctrl, Key::S) {
            self.save_project(false);
        }
        if pressed(ctrl, Key::O) {
            self.open_project_dialog();
        }
        if pressed(ctrl, Key::I) {
            self.import_dialog();
        }
        if pressed(ctrl, Key::N) {
            self.new_project();
        }
        if pressed(cs, Key::Z) || pressed(ctrl, Key::Y) {
            self.redo();
        } else if pressed(ctrl, Key::Z) {
            self.undo();
        }
        if pressed(ctrl, Key::A) {
            self.selection = self.project.timeline.all_clips().map(|(_, c)| c.id).collect();
        }
        if pressed(cs, Key::U) {
            self.ignore_grouping = !self.ignore_grouping;
        }
        if pressed(none, Key::Space) {
            if self.playing { self.stop() } else { self.play() }
        }
        if pressed(none, Key::Enter) || pressed(none, Key::K) {
            if self.playing { self.pause() }
        }
        if pressed(none, Key::L) {
            self.play();
        }
        if pressed(none, Key::S) {
            self.split();
        }
        if pressed(none, Key::Delete) || pressed(none, Key::Backspace) {
            self.delete_selection();
        }
        if pressed(none, Key::Escape) {
            self.selection.clear();
        }
        if pressed(none, Key::Q) {
            self.loop_play = !self.loop_play;
        }
        if pressed(none, Key::F8) {
            self.snapping = !self.snapping;
        }
        if pressed(none, Key::I) {
            self.set_region_edge(true);
        }
        if pressed(none, Key::O) {
            self.set_region_edge(false);
        }
        if pressed(none, Key::U) && !self.selection.is_empty() {
            self.checkpoint();
            let ids = self.grouped(&self.selection);
            self.project.timeline.ungroup(&ids);
        }
        if pressed(none, Key::G) && self.selection.len() > 1 {
            self.checkpoint();
            let ids = self.selection.clone();
            self.project.timeline.group(&ids);
        }
        let step = 1.0 / self.fps();
        if pressed(none, Key::ArrowRight) {
            self.seek(self.frame_snap(self.cursor) + step);
        }
        if pressed(none, Key::ArrowLeft) {
            self.seek((self.frame_snap(self.cursor) - step).max(0.0));
        }
        if pressed(ctrl, Key::ArrowRight) || pressed(ctrl, Key::ArrowLeft) {
            let fwd = ctx.input(|i| i.key_down(Key::ArrowRight));
            let mut edges = self.project.timeline.edges(&HashSet::new());
            edges.push(self.project.timeline.end());
            if let Some((a, b)) = self.project.timeline.region {
                edges.extend([a, b]);
            }
            let c = self.cursor;
            let t = if fwd {
                edges.into_iter().filter(|&e| e > c + 1e-6).fold(f64::MAX, f64::min)
            } else {
                edges.into_iter().filter(|&e| e < c - 1e-6).fold(f64::MIN, f64::max)
            };
            if t.is_finite() && t != f64::MAX && t != f64::MIN {
                self.seek(t);
            }
        }
        if pressed(none, Key::Home) {
            self.seek(0.0);
            self.view.reveal = true;
        }
        if pressed(none, Key::End) {
            let e = self.project.timeline.end();
            self.seek(e);
            self.view.reveal = true;
        }
        if pressed(none, Key::ArrowUp) {
            self.zoom_at(1.25, None);
        }
        if pressed(none, Key::ArrowDown) {
            self.zoom_at(0.8, None);
        }
        if pressed(none, Key::Backslash) {
            self.view.fit_pending = true;
        }
        let _ = shift;
        // Keyboard moves of the cursor bring it into view.
        if self.cursor != before {
            self.view.reveal = true;
        }
    }

    pub fn zoom_at(&mut self, factor: f32, anchor_t: Option<f64>) {
        let anchor = anchor_t.unwrap_or(self.cursor);
        let old = self.view.px_per_sec;
        let new = (old * factor).clamp(0.5, 4000.0);
        // keep anchor at the same screen x
        let x = (anchor - self.view.scroll) * old as f64;
        self.view.px_per_sec = new;
        self.view.scroll = (anchor - x / new as f64).max(0.0);
    }

    fn set_region_edge(&mut self, start: bool) {
        let c = self.cursor;
        let end = self.project.timeline.end().max(c);
        let (a, b) = self.project.timeline.region.unwrap_or((0.0, end));
        let r = if start { (c, if b > c { b } else { end.max(c + 1.0) }) } else { (if a < c { a } else { 0.0 }, c) };
        if r.1 - r.0 > model::MIN_CLIP {
            self.project.timeline.region = Some(r);
        }
    }

    // ───────────── panels ─────────────

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New project          Ctrl+N").clicked() {
                    self.new_project();
                }
                if ui.button("Open project…        Ctrl+O").clicked() {
                    self.open_project_dialog();
                }
                if ui.button("Save project         Ctrl+S").clicked() {
                    self.save_project(false);
                }
                if ui.button("Save project as…     Ctrl+Shift+S").clicked() {
                    self.save_project(true);
                }
                ui.separator();
                if ui.button("Import media…        Ctrl+I").clicked() {
                    self.import_dialog();
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo    Ctrl+Z")).clicked() {
                    self.undo();
                }
                if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("Redo    Ctrl+Y")).clicked() {
                    self.redo();
                }
                ui.separator();
                if ui.button("Split at cursor    S").clicked() {
                    self.split();
                }
                if ui.button("Delete             Del").clicked() {
                    self.delete_selection();
                }
                if ui.button("Ungroup            U").clicked() && !self.selection.is_empty() {
                    self.checkpoint();
                    let ids = self.grouped(&self.selection);
                    self.project.timeline.ungroup(&ids);
                }
                if ui.button("Group              G").clicked() && self.selection.len() > 1 {
                    self.checkpoint();
                    let ids = self.selection.clone();
                    self.project.timeline.group(&ids);
                }
                if ui.button("Clear loop region").clicked() {
                    self.project.timeline.region = None;
                }
            });
            ui.menu_button("View", |ui| {
                if ui.button("Zoom to fit          \\").clicked() {
                    self.view.fit_pending = true;
                }
                if ui.button("Zoom in              ↑").clicked() {
                    self.zoom_at(1.25, None);
                    self.view.reveal = true;
                }
                if ui.button("Zoom out             ↓").clicked() {
                    self.zoom_at(0.8, None);
                    self.view.reveal = true;
                }
            });
            ui.menu_button("Help", |ui| {
                ui.label(SHORTCUTS);
            });
            ui.separator();
            ui.toggle_value(&mut self.snapping, "🧲 Snap").on_hover_text("Snapping (F8)");
            ui.toggle_value(&mut self.ignore_grouping, "⛓ Ignore grouping").on_hover_text("Move/trim video and audio independently (Ctrl+Shift+U)");
            ui.toggle_value(&mut self.loop_play, "🔁 Loop").on_hover_text("Loop playback inside the region (Q)");
            if self.pending_probes > 0 {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(&self.status).color(crate::theme::DIM));
            });
        });
    }

    fn preview_panel(&mut self, ui: &mut Ui) {
        let avail = ui.available_rect_before_wrap();
        let transport_h = 40.0;
        let screen = egui::Rect::from_min_max(avail.min, egui::pos2(avail.max.x, avail.max.y - transport_h));
        ui.painter().rect_filled(screen, 0.0, Color32::BLACK);
        if let (Some(tex), true) = (&self.preview_tex, self.preview_has_image) {
            let sz = tex.size_vec2();
            let scale = (screen.width() / sz.x).min(screen.height() / sz.y);
            let r = egui::Rect::from_center_size(screen.center(), sz * scale);
            ui.painter().image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        } else if self.media.is_empty() {
            ui.painter().text(screen.center(), egui::Align2::CENTER_CENTER, "Drop video files anywhere", egui::FontId::proportional(20.0), crate::theme::DIM);
        }
        let bar = egui::Rect::from_min_max(egui::pos2(avail.min.x, screen.max.y), avail.max);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(egui::vec2(8.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        let ui = &mut child;
        let big = |s: &str| RichText::new(s).size(17.0);
        if ui.button(big("⏮")).on_hover_text("Go to start (Home)").clicked() {
            self.seek(0.0);
        }
        if ui.button(big("⏪")).on_hover_text("Previous frame (←)").clicked() {
            let t = (self.frame_snap(self.cursor) - 1.0 / self.fps()).max(0.0);
            self.seek(t);
        }
        if self.playing {
            if ui.button(big("⏸")).on_hover_text("Pause (Enter)").clicked() {
                self.pause();
            }
        } else if ui.button(big("▶")).on_hover_text("Play (Space)").clicked() {
            self.play();
        }
        if ui.button(big("⏹")).on_hover_text("Stop (Space) — returns to start position").clicked() {
            self.stop();
        }
        if ui.button(big("⏩")).on_hover_text("Next frame (→)").clicked() {
            let t = self.frame_snap(self.cursor) + 1.0 / self.fps();
            self.seek(t);
        }
        if ui.button(big("⏭")).on_hover_text("Go to end (End)").clicked() {
            let e = self.project.timeline.end();
            self.seek(e);
        }
        ui.toggle_value(&mut self.loop_play, big("🔁")).on_hover_text("Loop region (Q)");
        ui.add_space(12.0);
        ui.label(RichText::new(fmt_time(self.cursor)).monospace().size(20.0).color(crate::theme::ACCENT));
        ui.label(RichText::new(format!("/ {}", fmt_time(self.project.timeline.end()))).monospace().color(crate::theme::DIM));
        if let Some((a, b)) = self.project.timeline.region {
            ui.add_space(12.0);
            ui.label(RichText::new(format!("Region {} → {}  ({:.3}s)", fmt_time(a), fmt_time(b), b - a)).color(crate::theme::REGION));
        }
    }

    fn render_panel(&mut self, ui: &mut Ui) {
        let running = self.render_job.as_ref().is_some_and(|j| j.running());
        ui.add_space(4.0);
        ui.heading("Render");
        ui.add_space(4.0);
        let auto = model::auto_threads();
        ui.add_enabled_ui(!running, |ui| {
            let s = &mut self.project.settings;
            egui::Grid::new("render_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("CPU threads");
                ui.horizontal(|ui| {
                    let mut is_auto = s.threads == 0;
                    if ui.checkbox(&mut is_auto, format!("Auto ({auto})")).changed() {
                        s.threads = if is_auto { 0 } else { auto };
                    }
                    if !is_auto {
                        ui.add(egui::DragValue::new(&mut s.threads).range(1..=256));
                    }
                });
                ui.end_row();
                ui.label("Target size (MiB)");
                ui.add(egui::DragValue::new(&mut s.target_mib).range(0.1..=4096.0).speed(0.05).max_decimals(2));
                ui.end_row();
                ui.label("Resolution (height)");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.height).range(64..=4320).speed(2.0));
                    egui::ComboBox::from_id_salt("h_presets").selected_text("").width(18.0).show_ui(ui, |ui| {
                        for h in [360u32, 480, 540, 576, 720, 900, 1080, 1440] {
                            ui.selectable_value(&mut s.height, h, format!("{h}p"));
                        }
                    });
                });
                ui.end_row();
                ui.label("Framerate");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.fps).range(1..=240));
                    egui::ComboBox::from_id_salt("fps_presets").selected_text("").width(18.0).show_ui(ui, |ui| {
                        for f in [24u32, 25, 30, 48, 50, 60] {
                            ui.selectable_value(&mut s.fps, f, format!("{f} fps"));
                        }
                    });
                });
                ui.end_row();
                ui.label("Quality preset").on_hover_text("libvpx -speed for pass 2. 0 = slowest / best.");
                ui.add(egui::Slider::new(&mut s.quality_preset, 0..=8));
                ui.end_row();
                ui.label("Audio");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.audio, "");
                    ui.add_enabled(s.audio, egui::DragValue::new(&mut s.audio_kbps).range(6..=510).suffix(" kbps"));
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            egui::CollapsingHeader::new("Advanced encoder settings").show(ui, |ui| {
                egui::Grid::new("adv_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Pass 1 speed");
                    ui.add(egui::Slider::new(&mut s.pass1_speed, 0..=8));
                    ui.end_row();
                    ui.label("10-bit (profile 2)");
                    ui.checkbox(&mut s.ten_bit, "");
                    ui.end_row();
                    ui.label("Denoise (hqdn3d)");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut s.denoise, "");
                        ui.add_enabled(s.denoise, egui::TextEdit::singleline(&mut s.denoise_params).desired_width(80.0));
                    });
                    ui.end_row();
                    ui.label("aq-mode").on_hover_text("0 = stick to bitrate, 2 = efficient (may undershoot)");
                    ui.add(egui::Slider::new(&mut s.aq_mode, 0..=4));
                    ui.end_row();
                    ui.label("auto-alt-ref").on_hover_text("1 = stick to bitrate, up to 6 = more efficient for low motion");
                    ui.add(egui::Slider::new(&mut s.auto_alt_ref, 0..=6));
                    ui.end_row();
                    ui.label("lag-in-frames");
                    ui.add(egui::Slider::new(&mut s.lag_in_frames, 0..=25));
                    ui.end_row();
                    ui.label("Keyframe interval");
                    ui.add(egui::DragValue::new(&mut s.keyframe_sec).range(0.1..=60.0).speed(0.1).suffix(" s"));
                    ui.end_row();
                    ui.label("Tiles (cols/rows log2)");
                    ui.horizontal(|ui| {
                        let mut auto_t = s.tiles.is_none();
                        if ui.checkbox(&mut auto_t, "Auto").changed() {
                            s.tiles = if auto_t { None } else { Some(s.effective_tiles()) };
                        }
                        if let Some((c, r)) = &mut s.tiles {
                            ui.add(egui::DragValue::new(c).range(0..=6));
                            ui.add(egui::DragValue::new(r).range(0..=2));
                        } else {
                            let (c, r) = s.effective_tiles();
                            ui.label(RichText::new(format!("{c} / {r}")).color(crate::theme::DIM));
                        }
                    });
                    ui.end_row();
                    ui.label("Safety margin");
                    ui.add(egui::DragValue::new(&mut s.margin_pct).range(0.0..=50.0).speed(0.1).suffix(" %"));
                    ui.end_row();
                    ui.label("ffmpeg.exe");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut s.ffmpeg_path).hint_text("auto").desired_width(120.0));
                        if ui.small_button("…").clicked()
                            && let Some(p) = rfd::FileDialog::new().add_filter("ffmpeg", &["exe"]).pick_file()
                        {
                            s.ffmpeg_path = p.to_string_lossy().into_owned();
                        }
                    });
                    ui.end_row();
                });
                if ui.button("Reset to defaults").clicked() {
                    *s = RenderSettings::default();
                }
            });
        });

        ui.separator();
        // ---- summary ----
        let (r0, r1) = self.project.timeline.render_range();
        let dur = (r1 - r0).max(0.0);
        let s = &self.project.settings;
        let src = if self.project.timeline.region.is_some() { "Loop region" } else { "Whole timeline" };
        egui::Grid::new("summary").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
            ui.label("Range");
            ui.label(RichText::new(format!("{src}: {} → {}", fmt_time(r0), fmt_time(r1))).color(crate::theme::REGION));
            ui.end_row();
            ui.label("Duration");
            ui.label(format!("{dur:.3} s"));
            ui.end_row();
            ui.label("Video bitrate");
            ui.label(RichText::new(format!("{} kbps", s.video_kbps(dur))).strong());
            ui.end_row();
            ui.label("Threads / tiles");
            let (c, r) = s.effective_tiles();
            ui.label(format!("{} / {c}×{r}", s.effective_threads()));
            ui.end_row();
        });

        ui.separator();
        ui.label("Output");
        let auto_out = render::auto_output(&self.project);
        let shown = self.project.output.clone().or(auto_out);
        ui.horizontal(|ui| {
            let text = shown.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(import a clip first)".into());
            ui.add(egui::Label::new(RichText::new(text).small()).truncate()).on_hover_text("Default: next to the source");
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!running, egui::Button::new("Change…")).clicked() {
                let mut d = rfd::FileDialog::new().add_filter("WebM", &["webm"]);
                if let Some(p) = &shown {
                    if let Some(dir) = p.parent() {
                        d = d.set_directory(dir);
                    }
                    if let Some(n) = p.file_name() {
                        d = d.set_file_name(n.to_string_lossy());
                    }
                }
                if let Some(p) = d.save_file() {
                    self.project.output = Some(p);
                }
            }
            if self.project.output.is_some() && ui.add_enabled(!running, egui::Button::new("Auto")).on_hover_text("Back to next-to-source").clicked() {
                self.project.output = None;
            }
        });
        ui.add_space(8.0);

        // ---- render button / progress ----
        if running {
            let st = self.render_job.as_ref().unwrap().status.lock().clone();
            let pass = if let Phase::Pass(p) = st.phase { p } else { 2 };
            let el = st.started.elapsed().as_secs_f64();
            let eta = if st.progress > 0.02 { el / st.progress as f64 - el } else { f64::NAN };
            ui.add(egui::ProgressBar::new(st.progress).show_percentage().animate(true));
            ui.label(format!("Pass {pass}/2 · {:.0}s elapsed{}", el, if eta.is_finite() { format!(" · ~{eta:.0}s left") } else { String::new() }));
            if ui.button("Cancel").clicked() {
                self.render_job.as_ref().unwrap().cancel();
            }
        } else {
            let can = dur > 0.1;
            let btn = egui::Button::new(RichText::new("⏺  RENDER").size(18.0).strong().color(Color32::WHITE))
                .fill(crate::theme::RENDER_BTN)
                .min_size(egui::vec2(ui.available_width(), 38.0));
            if ui.add_enabled(can, btn).clicked() {
                self.start_render();
            }
            ui.horizontal(|ui| {
                if ui.small_button("Copy ffmpeg commands").clicked() {
                    match render::plan(&self.project, &self.media) {
                        Ok(p) => {
                            ui.ctx().copy_text(render::describe(&p));
                            let _ = std::fs::remove_dir_all(&p.workdir);
                            self.status = "ffmpeg commands copied to clipboard".into();
                        }
                        Err(e) => self.render_error = Some(e.to_string()),
                    }
                }
            });
        }
        if let Some(job) = &self.render_job {
            let st = job.status.lock().clone();
            match &st.phase {
                Phase::Done { bytes } => {
                    let mib = *bytes as f64 / 1048576.0;
                    let ok = *bytes <= st.target_bytes;
                    ui.label(
                        RichText::new(format!("✔ Done in {:.0}s — {mib:.2} MiB (target {:.2})", st.started.elapsed().as_secs_f64(), st.target_bytes as f64 / 1048576.0))
                            .color(if ok { crate::theme::OK } else { crate::theme::WARN }),
                    );
                    if !ok {
                        ui.label(RichText::new("Over target — lower the size a bit or add a safety margin.").color(crate::theme::WARN));
                    }
                    if ui.button("Show in folder").clicked() {
                        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", st.output.display())).spawn();
                    }
                }
                Phase::Failed(e) => {
                    ui.label(RichText::new(e).color(crate::theme::ERR).small());
                }
                Phase::Cancelled => {
                    ui.label(RichText::new("Render cancelled").color(crate::theme::DIM));
                }
                Phase::Pass(_) => {}
            }
        }
        if let Some(e) = &self.render_error {
            ui.label(RichText::new(e).color(crate::theme::ERR));
        }
    }

    fn start_render(&mut self) {
        self.render_error = None;
        if self.playing {
            self.pause();
        }
        match render::plan(&self.project, &self.media) {
            Ok(plan) => {
                self.status = format!("Rendering {}×{} @ {} kbps → {}", plan.out_w, plan.out_h, plan.video_kbps, plan.output.display());
                let ctx = self.ctx.clone();
                self.render_job = Some(RenderJob::start(plan, self.project.settings.target_mib, move || ctx.request_repaint()));
            }
            Err(e) => self.render_error = Some(e.to_string()),
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context, timeline_drop_t: Option<f64>) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        let mut at = timeline_drop_t;
        for f in dropped {
            let p = f.path().to_path_buf();
            if !p.as_os_str().is_empty() {
                if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wsproj")) {
                    self.open_project(&p);
                    continue;
                }
                self.import(p, at);
                at = None; // further files append at the end
            }
        }
    }

    fn persist_settings(&mut self) {
        let s = &self.project.settings;
        if serde_json::to_string(s).ok() != serde_json::to_string(&self.saved_settings).ok() {
            self.saved_settings = s.clone();
            if let Some(p) = settings_file() {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, serde_json::to_string_pretty(s).unwrap_or_default());
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_jobs();
        self.shortcuts(&ctx);
        self.update_transport();

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::right("render").default_size(330.0).min_size(280.0).resizable(true).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.render_panel(ui));
        });
        let tl_resp = egui::Panel::bottom("timeline")
            .default_size(320.0)
            .min_size(180.0)
            .resizable(true)
            .frame(egui::Frame::new().fill(crate::theme::TIMELINE_BG))
            .show(ui, |ui| self.timeline_ui(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(crate::theme::PANEL_BG)).show(ui, |ui| self.preview_panel(ui));

        let drop_t = tl_resp.inner;
        if ctx.input(|i| !i.raw.dropped_files.is_empty()) {
            self.handle_drops(&ctx, drop_t);
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")))
                .rect_stroke(screen.shrink(2.0), 4.0, egui::Stroke::new(3.0, crate::theme::ACCENT), egui::StrokeKind::Inside);
        }
        self.persist_settings();
    }
}

pub const SHORTCUTS: &str = "\
Space        Play / Stop (returns to start)
Enter / K    Pause here
L            Play
← / →        Previous / next frame
Ctrl+← / →   Previous / next edit point
Home / End   Start / end of timeline
S            Split at cursor (selected, or all)
Del          Delete selected
I / O        Set region start / end at cursor
Q            Loop playback in region
U / G        Ungroup / group selected
Ctrl+Shift+U Ignore grouping
F8           Snapping
Wheel / ↑ ↓  Zoom in / out (the wheel zooms around the mouse)
\\            Zoom to fit the whole timeline
Shift+wheel  Scroll timeline (or middle-drag, or the scroll bar)
Scroll bar   Drag to scroll, drag its ends to zoom, double-click to fit
Drag in region bar or empty track space   Set region
Drag region pins / bar                    Resize / move region
Double-click clip     Region = clip
Edit ▸ Clear loop region                  Remove region
Ctrl+Z / Ctrl+Y       Undo / Redo
Ctrl+S / Ctrl+O       Save / Open project
Ctrl+I                Import media";
