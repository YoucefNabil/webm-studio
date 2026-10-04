//! Preview video engine: a decode thread that turns timeline time into RGBA frames.
//! Seeks are coalesced (only the latest scrub position is decoded); during playback it
//! decodes ahead into a small bounded queue.

use crate::media::{RgbaFrame, VideoDecoder};
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct VClip {
    pub path: Arc<PathBuf>,
    pub start: f64,
    pub src_in: f64,
    pub src_out: f64,
}

impl VClip {
    fn end(&self) -> f64 {
        self.start + self.src_out - self.src_in
    }
}

pub struct VFrame {
    pub generation: u64,
    /// Timeline time at which this frame becomes visible.
    pub t: f64,
    /// None = black (gap).
    pub image: Option<RgbaFrame>,
}

enum Cmd {
    Timeline(Arc<Vec<VClip>>),
    Seek(u64, f64),
    /// Fast approximate seek while dragging; refined to the exact frame once idle.
    Scrub(u64, f64),
    Play(u64, f64),
    Stop,
}

pub struct VideoEngine {
    tx: Sender<Cmd>,
    pub frames: Receiver<VFrame>,
    generation: u64,
}

const PREVIEW_W: u32 = 1280;
const PREVIEW_H: u32 = 720;
/// How long the scrub position must stay still before the exact frame is decoded.
const REFINE_AFTER: std::time::Duration = std::time::Duration::from_millis(35);
const GAP_STEP: f64 = 1.0 / 30.0;

impl VideoEngine {
    pub fn new(repaint: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = unbounded();
        let (ftx, frx) = bounded(6);
        let drain = frx.clone();
        std::thread::Builder::new().name("video".into()).spawn(move || Worker::new(rx, ftx, drain, repaint).run()).unwrap();
        Self { tx, frames: frx, generation: 0 }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn set_timeline(&self, clips: Vec<VClip>) {
        let _ = self.tx.send(Cmd::Timeline(Arc::new(clips)));
    }

    pub fn seek(&mut self, t: f64) {
        self.generation += 1;
        let _ = self.tx.send(Cmd::Seek(self.generation, t));
    }

    pub fn scrub(&mut self, t: f64) {
        self.generation += 1;
        let _ = self.tx.send(Cmd::Scrub(self.generation, t));
    }

    pub fn play(&mut self, t: f64) {
        self.generation += 1;
        let _ = self.tx.send(Cmd::Play(self.generation, t));
    }

    pub fn stop(&mut self) {
        self.generation += 1;
        let _ = self.tx.send(Cmd::Stop);
    }
}

struct Worker<F: Fn()> {
    rx: Receiver<Cmd>,
    tx: Sender<VFrame>,
    /// Clone of the frame receiver, used to throw away stale frames on seek/stop.
    drain: Receiver<VFrame>,
    repaint: F,
    clips: Arc<Vec<VClip>>,
    decoders: HashMap<Arc<PathBuf>, VideoDecoder>,
    lru: Vec<Arc<PathBuf>>,
    generation: u64,
    playing: bool,
    play_t: f64,
    /// Clip index currently streaming sequentially during playback.
    play_clip: Option<usize>,
    threads: usize,
    /// Exact position still owed after a fast scrub frame.
    refine: Option<f64>,
}

impl<F: Fn()> Worker<F> {
    fn new(rx: Receiver<Cmd>, tx: Sender<VFrame>, drain: Receiver<VFrame>, repaint: F) -> Self {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(16);
        Self { rx, tx, drain, repaint, clips: Arc::new(Vec::new()), decoders: HashMap::new(), lru: Vec::new(), generation: 0, playing: false, play_t: 0.0, play_clip: None, threads, refine: None }
    }

    fn decoder(&mut self, path: &Arc<PathBuf>) -> Option<&mut VideoDecoder> {
        if !self.decoders.contains_key(path) {
            let d = VideoDecoder::open(path, PREVIEW_W, PREVIEW_H, self.threads).ok()?;
            self.decoders.insert(path.clone(), d);
            self.lru.push(path.clone());
            if self.lru.len() > 4 {
                let old = self.lru.remove(0);
                self.decoders.remove(&old);
            }
        } else if let Some(i) = self.lru.iter().position(|p| p == path) {
            let p = self.lru.remove(i);
            self.lru.push(p);
        }
        self.decoders.get_mut(path)
    }

    fn clip_at(&self, t: f64) -> Option<usize> {
        self.clips.iter().position(|c| t >= c.start - 1e-6 && t < c.end() - 1e-6)
    }

    fn handle(&mut self, cmd: Cmd, pending_seek: &mut Option<(f64, bool)>) {
        if !matches!(cmd, Cmd::Timeline(_)) {
            while self.drain.try_recv().is_ok() {}
            self.refine = None;
        }
        match cmd {
            Cmd::Timeline(c) => {
                self.clips = c;
                self.play_clip = None;
            }
            Cmd::Seek(g, t) => {
                self.generation = g;
                self.playing = false;
                *pending_seek = Some((t, false));
            }
            Cmd::Scrub(g, t) => {
                self.generation = g;
                self.playing = false;
                *pending_seek = Some((t, true));
            }
            Cmd::Play(g, t) => {
                self.generation = g;
                self.playing = true;
                self.play_t = t;
                self.play_clip = None;
                *pending_seek = None;
            }
            Cmd::Stop => {
                self.playing = false;
                *pending_seek = None;
            }
        }
    }

    /// Frame at timeline time `t`. With `fast`, a backward or far jump shows the nearest
    /// keyframe instead and schedules an exact refine.
    fn render_at(&mut self, t: f64, fast: bool) -> VFrame {
        let generation = self.generation;
        let mut refine = false;
        let image = match self.clip_at(t) {
            Some(i) => {
                let c = self.clips[i].clone();
                let src = c.src_in + (t - c.start);
                self.decoder(&c.path).and_then(|d| {
                    let ahead = src - d.last_t;
                    if fast && !(ahead > 0.0 && ahead < 0.6) {
                        refine = true;
                        d.keyframe_near(src)
                    } else {
                        d.frame_at(src)
                    }
                })
            }
            None => None,
        };
        self.refine = if refine { Some(t) } else { None };
        VFrame { generation, t, image }
    }

    /// Produce the next playback frame and advance `play_t`.
    fn next_play_frame(&mut self) -> Option<VFrame> {
        let generation = self.generation;
        let t = self.play_t;
        let Some(i) = self.clip_at(t) else {
            // Gap: black until the next clip starts.
            let next = self.clips.iter().map(|c| c.start).filter(|&s| s > t + 1e-6).fold(f64::MAX, f64::min);
            if next == f64::MAX && t > self.clips.iter().map(|c| c.end()).fold(0.0, f64::max) + 1.0 {
                // Past the end: emit nothing more (the UI stops the transport).
                self.play_t += GAP_STEP;
                return Some(VFrame { generation, t, image: None });
            }
            self.play_t = (t + GAP_STEP).min(next);
            self.play_clip = None;
            return Some(VFrame { generation, t, image: None });
        };
        let c = self.clips[i].clone();
        let continuing = self.play_clip == Some(i);
        let d = self.decoder(&c.path)?;
        let img = if continuing { d.next() } else { d.frame_at(c.src_in + (t - c.start)) };
        match img {
            Some(f) if f.t < c.src_out - 1e-6 => {
                let ft = (c.start + (f.t - c.src_in)).max(c.start);
                self.play_t = (ft + d.frame_dur()).max(t + 1e-4);
                self.play_clip = Some(i);
                Some(VFrame { generation, t: ft, image: Some(f) })
            }
            _ => {
                // Source exhausted or passed the out-point: jump to the clip end.
                self.play_t = c.end() + 1e-6;
                self.play_clip = None;
                self.next_play_frame()
            }
        }
    }

    fn run(mut self) {
        let mut pending_seek: Option<(f64, bool)> = None;
        loop {
            // Drain all queued commands; keep only the latest seek.
            if !self.playing && pending_seek.is_none() {
                let got = if let Some(t) = self.refine {
                    match self.rx.recv_timeout(REFINE_AFTER) {
                        Ok(c) => Some(c),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            pending_seek = Some((t, false));
                            None
                        }
                        Err(_) => return,
                    }
                } else {
                    match self.rx.recv() {
                        Ok(c) => Some(c),
                        Err(_) => return,
                    }
                };
                if let Some(c) = got {
                    self.handle(c, &mut pending_seek);
                }
            }
            while let Ok(c) = self.rx.try_recv() {
                self.handle(c, &mut pending_seek);
            }
            if let Some((t, fast)) = pending_seek.take() {
                let f = self.render_at(t, fast);
                let _ = self.tx.send(f);
                (self.repaint)();
                continue;
            }
            if self.playing {
                let Some(f) = self.next_play_frame() else {
                    self.playing = false;
                    continue;
                };
                // Wait for queue space, but stay responsive to commands.
                select! {
                    send(self.tx, f) -> _ => {}
                    recv(self.rx) -> c => match c {
                        Ok(c) => self.handle(c, &mut pending_seek),
                        Err(_) => return,
                    }
                }
            }
        }
    }
}
