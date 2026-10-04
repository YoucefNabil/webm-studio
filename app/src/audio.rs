//! Preview audio: a mixer thread decodes every audio track at the current timeline time,
//! sums them, and feeds a cpal output stream. Samples actually consumed by the device
//! drive the playback clock, so video follows audio.

use crate::media::AudioReader;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct AClip {
    pub id: u64,
    pub path: Arc<PathBuf>,
    pub stream_index: usize,
    pub start: f64,
    pub src_in: f64,
    pub src_out: f64,
}

#[derive(Clone, Debug)]
pub struct ATrack {
    pub gain: f32,
    pub clips: Vec<AClip>,
}

enum Cmd {
    Tracks(Arc<Vec<ATrack>>),
    Play(f64),
    Stop,
}

struct Shared {
    buf: Mutex<VecDeque<f32>>,
    /// Stereo frames of real (non-padding) audio consumed since Play.
    played: AtomicU64,
    active: AtomicBool,
}

pub struct AudioEngine {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    pub rate: u32,
    _stream: Option<cpal::Stream>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let shared = Arc::new(Shared { buf: Mutex::new(VecDeque::new()), played: AtomicU64::new(0), active: AtomicBool::new(false) });
        let (stream, rate) = match open_output(shared.clone()) {
            Some((s, r)) => (Some(s), r),
            None => (None, 48000),
        };
        let (tx, rx) = unbounded();
        let sh = shared.clone();
        std::thread::Builder::new().name("audio-mix".into()).spawn(move || mixer(rx, sh, rate)).unwrap();
        Self { tx, shared, rate, _stream: stream }
    }

    pub fn has_device(&self) -> bool {
        self._stream.is_some()
    }

    pub fn set_tracks(&self, tracks: Vec<ATrack>) {
        let _ = self.tx.send(Cmd::Tracks(Arc::new(tracks)));
    }

    pub fn play(&self, t: f64) {
        self.shared.active.store(false, Ordering::SeqCst);
        self.shared.buf.lock().clear();
        self.shared.played.store(0, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Play(t));
    }

    pub fn stop(&self) {
        self.shared.active.store(false, Ordering::SeqCst);
        self.shared.buf.lock().clear();
        let _ = self.tx.send(Cmd::Stop);
    }

    /// Seconds of audio played since the last `play`.
    pub fn played_secs(&self) -> f64 {
        self.shared.played.load(Ordering::Relaxed) as f64 / self.rate as f64
    }
}

fn open_output(shared: Arc<Shared>) -> Option<(cpal::Stream, u32)> {
    let host = cpal::default_host();
    let dev = host.default_output_device()?;
    let cfg = dev.default_output_config().ok()?;
    let channels = cfg.channels() as usize;
    let rate = cfg.sample_rate();
    let rate: u32 = rate.into();
    let config: cpal::StreamConfig = cfg.config();
    let err = |e| eprintln!("audio stream error: {e}");
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => dev
            .build_output_stream(config.clone(), move |out: &mut [f32], _| fill(out, channels, &shared, |s| s), err, None)
            .ok()?,
        cpal::SampleFormat::I16 => dev
            .build_output_stream(config, move |out: &mut [i16], _| fill(out, channels, &shared, |s| (s * 32767.0) as i16), err, None)
            .ok()?,
        _ => return None,
    };
    stream.play().ok()?;
    Some((stream, rate))
}

fn fill<T: Copy + Default>(out: &mut [T], channels: usize, sh: &Shared, conv: impl Fn(f32) -> T) {
    let active = sh.active.load(Ordering::Relaxed);
    let mut buf = sh.buf.lock();
    let mut real = 0u64;
    for frame in out.chunks_mut(channels) {
        let (l, r) = if active && buf.len() >= 2 {
            real += 1;
            (buf.pop_front().unwrap(), buf.pop_front().unwrap())
        } else {
            (0.0, 0.0)
        };
        for (c, o) in frame.iter_mut().enumerate() {
            *o = conv(match c {
                0 => l,
                1 => r,
                _ => 0.0,
            });
        }
    }
    sh.played.fetch_add(real, Ordering::Relaxed);
}

fn mixer(rx: Receiver<Cmd>, sh: Arc<Shared>, rate: u32) {
    const BLOCK: usize = 1024;
    let target_fill = (rate as usize * 2) / 6; // ~170 ms of stereo samples
    let mut tracks: Arc<Vec<ATrack>> = Arc::new(Vec::new());
    let mut playing = false;
    let mut pos: u64 = 0; // timeline position in frames
    // Per track: (clip id, next timeline frame the reader will produce, reader)
    let mut readers: Vec<Option<(u64, u64, AudioReader)>> = Vec::new();
    let mut mix = vec![0f32; BLOCK * 2];
    loop {
        let cmd = if playing { rx.try_recv().ok() } else { rx.recv().ok() };
        if let Some(c) = cmd {
            match c {
                Cmd::Tracks(t) => {
                    tracks = t;
                    readers.clear();
                }
                Cmd::Play(t) => {
                    sh.buf.lock().clear();
                    sh.played.store(0, Ordering::SeqCst);
                    pos = (t.max(0.0) * rate as f64).round() as u64;
                    readers.clear();
                    playing = true;
                }
                Cmd::Stop => {
                    playing = false;
                    sh.buf.lock().clear();
                }
            }
            continue;
        }
        if !playing {
            continue;
        }
        if sh.buf.lock().len() >= target_fill {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        readers.resize_with(tracks.len(), || None);
        mix.iter_mut().for_each(|s| *s = 0.0);
        let b0 = pos;
        let b1 = pos + BLOCK as u64;
        for (ti, tr) in tracks.iter().enumerate() {
            if tr.gain <= 0.0 {
                continue;
            }
            for c in &tr.clips {
                let cs = (c.start * rate as f64).round() as u64;
                let ce = ((c.start + c.src_out - c.src_in) * rate as f64).round() as u64;
                if ce <= b0 || cs >= b1 {
                    continue;
                }
                let s0 = cs.max(b0);
                let s1 = ce.min(b1);
                let fresh = !matches!(&readers[ti], Some((id, next, _)) if *id == c.id && *next == s0);
                if fresh {
                    let at = c.src_in + (s0 as f64 / rate as f64 - c.start);
                    readers[ti] = AudioReader::open(&c.path, c.stream_index, at.max(0.0), rate, 2).ok().map(|r| (c.id, s0, r));
                }
                if let Some((_, next, r)) = readers[ti].as_mut() {
                    let a = ((s0 - b0) * 2) as usize;
                    let b = ((s1 - b0) * 2) as usize;
                    r.mix_into(&mut mix[a..b], tr.gain);
                    *next = s1;
                }
            }
        }
        pos = b1;
        let mut buf = sh.buf.lock();
        buf.extend(mix.iter().map(|s| s.clamp(-1.0, 1.0)));
        sh.active.store(true, Ordering::Relaxed);
    }
}
