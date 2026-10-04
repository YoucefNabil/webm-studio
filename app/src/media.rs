//! FFmpeg (libav*) access: probing, a seekable video decoder that outputs RGBA,
//! an audio reader that outputs interleaved stereo f32, plus waveform/thumbnail jobs.

use anyhow::{Context as _, Result, anyhow};
use ffmpeg_next as ff;
use ff::{format, frame, media, software};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
#[allow(dead_code)] // informational, shown in debug output
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub codec: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct AudioInfo {
    /// Absolute stream index inside the container.
    pub index: usize,
    pub channels: u16,
    pub rate: u32,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub duration: f64,
    pub video: Option<VideoInfo>,
    pub audio: Vec<AudioInfo>,
}

impl MediaInfo {
    pub fn name(&self) -> String {
        self.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

fn q(r: ff::Rational) -> f64 {
    if r.denominator() == 0 { 0.0 } else { r.numerator() as f64 / r.denominator() as f64 }
}

/// Seconds offset of a stream's first timestamp.
fn stream_start(s: &ff::format::stream::Stream) -> f64 {
    let st = s.start_time();
    if st == ff::ffi::AV_NOPTS_VALUE { 0.0 } else { st as f64 * q(s.time_base()) }
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    let ictx = format::input(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut duration = if ictx.duration() > 0 { ictx.duration() as f64 / 1e6 } else { 0.0 };
    let mut video = None;
    let mut audio = Vec::new();
    for s in ictx.streams() {
        let par = s.parameters();
        let sdur = if s.duration() > 0 { s.duration() as f64 * q(s.time_base()) } else { 0.0 };
        match par.medium() {
            media::Type::Video if video.is_none() => {
                // Skip embedded cover art.
                if s.disposition().contains(format::stream::Disposition::ATTACHED_PIC) {
                    continue;
                }
                let dec = ff::codec::Context::from_parameters(par)?.decoder().video()?;
                let mut fps = q(s.avg_frame_rate());
                if !(1.0..=1000.0).contains(&fps) {
                    fps = q(s.rate());
                }
                if !(1.0..=1000.0).contains(&fps) {
                    fps = 30.0;
                }
                video = Some(VideoInfo {
                    width: dec.width(),
                    height: dec.height(),
                    fps,
                    codec: dec.codec().map(|c| c.name().to_string()).unwrap_or_default(),
                });
                if duration <= 0.0 {
                    duration = sdur;
                }
            }
            media::Type::Audio => {
                let dec = ff::codec::Context::from_parameters(par)?.decoder().audio()?;
                let meta = s.metadata();
                let title = meta.get("title").or_else(|| meta.get("handler_name")).map(str::to_string)
                    .filter(|t| !t.is_empty() && !t.starts_with("SoundHandle") && !t.contains("Sound Media"));
                audio.push(AudioInfo { index: s.index(), channels: dec.channels(), rate: dec.rate(), title });
                if duration <= 0.0 {
                    duration = sdur;
                }
            }
            _ => {}
        }
    }
    if video.is_none() && audio.is_empty() {
        return Err(anyhow!("no audio or video streams"));
    }
    Ok(MediaInfo { path: path.to_path_buf(), duration, video, audio })
}

// ───────────────────────────── video ─────────────────────────────

pub struct RgbaFrame {
    pub w: usize,
    pub h: usize,
    pub pixels: Vec<u8>,
    /// Source time (seconds, relative to stream start).
    pub t: f64,
}

pub struct VideoDecoder {
    ictx: format::context::Input,
    stream: usize,
    tb: f64,
    start: f64,
    dec: ff::decoder::Video,
    scaler: Option<(ff::format::Pixel, u32, u32, software::scaling::Context)>,
    out_w: u32,
    out_h: u32,
    frame_dur: f64,
    eof_sent: bool,
    /// Time of the last frame returned (for sequential reads).
    pub last_t: f64,
    /// Decoding on the GPU (D3D11VA).
    #[allow(dead_code)]
    pub hw: bool,
}

/// get_format callback: accept only D3D11 surfaces, so a missing hwaccel fails loudly
/// and we fall back to software decoding.
unsafe extern "C" fn pick_d3d11(_: *mut ff::ffi::AVCodecContext, mut fmts: *const ff::ffi::AVPixelFormat) -> ff::ffi::AVPixelFormat {
    use ff::ffi::AVPixelFormat::*;
    unsafe {
        while *fmts != AV_PIX_FMT_NONE {
            if *fmts == AV_PIX_FMT_D3D11 {
                return AV_PIX_FMT_D3D11;
            }
            fmts = fmts.add(1);
        }
    }
    AV_PIX_FMT_NONE
}

impl VideoDecoder {
    /// `max_w`/`max_h` bound the RGBA output size (aspect preserved).
    /// Tries GPU decoding first, then software.
    pub fn open(path: &Path, max_w: u32, max_h: u32, threads: usize) -> Result<Self> {
        if std::env::var_os("WS_NO_HW").is_none()
            && let Ok(d) = Self::open_with(path, max_w, max_h, threads, true)
        {
            return Ok(d);
        }
        Self::open_with(path, max_w, max_h, threads, false)
    }

    fn open_with(path: &Path, max_w: u32, max_h: u32, threads: usize, hw: bool) -> Result<Self> {
        let ictx = format::input(path)?;
        let s = ictx.streams().best(media::Type::Video).ok_or_else(|| anyhow!("no video stream"))?;
        let stream = s.index();
        let tb = q(s.time_base());
        let start = stream_start(&s);
        let mut fps = q(s.avg_frame_rate());
        if !(1.0..=1000.0).contains(&fps) {
            fps = 30.0;
        }
        let mut cctx = ff::codec::Context::from_parameters(s.parameters())?;
        let mut codec = None;
        if hw {
            unsafe {
                let mut dev = std::ptr::null_mut();
                if ff::ffi::av_hwdevice_ctx_create(&mut dev, ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, std::ptr::null(), std::ptr::null_mut(), 0) < 0 {
                    return Err(anyhow!("no D3D11VA device"));
                }
                let p = cctx.as_mut_ptr();
                (*p).hw_device_ctx = dev; // ownership moves to the codec context
                (*p).get_format = Some(pick_d3d11);
            }
            // libdav1d (the default AV1 decoder) has no hwaccel; the native one does.
            if cctx.id() == ff::codec::Id::AV1 {
                codec = ff::decoder::find_by_name("av1");
            }
        } else {
            cctx.set_threading(ff::codec::threading::Config {
                kind: ff::codec::threading::Type::Frame,
                count: threads,
                ..Default::default()
            });
        }
        let dec = match codec {
            Some(c) => cctx.decoder().open_as(c)?.video()?,
            None => cctx.decoder().video()?,
        };
        let (sw, sh) = (dec.width().max(2), dec.height().max(2));
        let scale = (max_w as f64 / sw as f64).min(max_h as f64 / sh as f64).min(1.0);
        let out_w = ((sw as f64 * scale) as u32).max(2) & !1;
        let out_h = ((sh as f64 * scale) as u32).max(2) & !1;
        let mut d = Self { ictx, stream, tb, start, dec, scaler: None, out_w, out_h, frame_dur: 1.0 / fps, eof_sent: false, last_t: f64::NEG_INFINITY, hw };
        if hw {
            // Prove the GPU path works on this stream, then rewind.
            let f = d.next_raw().ok_or_else(|| anyhow!("hw decode failed"))?;
            d.to_rgba(&f, 0.0).ok_or_else(|| anyhow!("hw transfer failed"))?;
            d.rewind_to(0.0);
        }
        Ok(d)
    }

    fn rewind_to(&mut self, t: f64) {
        let ts = ((t + self.start).max(0.0) / self.tb) as i64;
        unsafe {
            ff::ffi::av_seek_frame(self.ictx.as_mut_ptr(), self.stream as i32, ts, ff::ffi::AVSEEK_FLAG_BACKWARD as i32);
        }
        self.dec.flush();
        self.eof_sent = false;
        self.last_t = f64::NEG_INFINITY;
    }

    pub fn frame_dur(&self) -> f64 {
        self.frame_dur
    }

    fn frame_time(&self, f: &frame::Video) -> f64 {
        f.timestamp().or(f.pts()).map(|p| p as f64 * self.tb - self.start).unwrap_or(self.last_t + self.frame_dur)
    }

    /// Decode the next frame in stream order. Returns None at end of stream.
    fn next_raw(&mut self) -> Option<frame::Video> {
        let mut f = frame::Video::empty();
        loop {
            if self.dec.receive_frame(&mut f).is_ok() {
                return Some(f);
            }
            if self.eof_sent {
                return None;
            }
            let mut sent = false;
            for (s, p) in self.ictx.packets() {
                if s.index() == self.stream {
                    let _ = self.dec.send_packet(&p);
                    sent = true;
                    break;
                }
            }
            if !sent {
                let _ = self.dec.send_eof();
                self.eof_sent = true;
            }
        }
    }

    fn to_rgba(&mut self, f: &frame::Video, t: f64) -> Option<RgbaFrame> {
        let downloaded;
        let f = if unsafe { (*f.as_ptr()).format } == ff::ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
            let mut sw = frame::Video::empty();
            if unsafe { ff::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), f.as_ptr(), 0) } < 0 {
                return None;
            }
            downloaded = sw;
            &downloaded
        } else {
            f
        };
        let key = (f.format(), f.width(), f.height());
        if self.scaler.as_ref().is_none_or(|(p, w, h, _)| (*p, *w, *h) != key) {
            let ctx = software::scaling::Context::get(
                f.format(), f.width(), f.height(),
                ff::format::Pixel::RGBA, self.out_w, self.out_h,
                software::scaling::Flags::BILINEAR,
            ).ok()?;
            self.scaler = Some((key.0, key.1, key.2, ctx));
        }
        let mut out = frame::Video::empty();
        self.scaler.as_mut()?.3.run(f, &mut out).ok()?;
        let (w, h) = (self.out_w as usize, self.out_h as usize);
        let stride = out.stride(0);
        let data = out.data(0);
        let mut pixels = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            pixels.extend_from_slice(&data[y * stride..y * stride + w * 4]);
        }
        Some(RgbaFrame { w, h, pixels, t })
    }

    /// Next frame in order, converted.
    pub fn next(&mut self) -> Option<RgbaFrame> {
        let f = self.next_raw()?;
        let t = self.frame_time(&f);
        self.last_t = t;
        self.to_rgba(&f, t)
    }

    /// Frame displayed at source time `t` (frame-accurate). Decodes forward instead of
    /// seeking when the target is a little ahead of the current position.
    pub fn frame_at(&mut self, t: f64) -> Option<RgbaFrame> {
        let ahead = t - self.last_t;
        if !(ahead > -self.frame_dur * 0.5 && ahead < 1.5) {
            // Seek on the video stream itself, to the keyframe at or before the target.
            self.rewind_to(t - 0.05);
        }
        let mut best: Option<(frame::Video, f64)> = None;
        loop {
            match self.next_raw() {
                Some(f) => {
                    let ft = self.frame_time(&f);
                    self.last_t = ft;
                    // A frame is shown from its timestamp until the next one.
                    if ft > t + self.frame_dur * 0.5 {
                        let (bf, bt) = best.unwrap_or((f, ft));
                        return self.to_rgba(&bf, bt);
                    }
                    best = Some((f, ft));
                    if ft + self.frame_dur * 0.5 > t {
                        let (bf, bt) = best.take().unwrap();
                        return self.to_rgba(&bf, bt);
                    }
                }
                None => {
                    let (bf, bt) = best?;
                    return self.to_rgba(&bf, bt);
                }
            }
        }
    }

    /// Fast approximate frame (nearest keyframe at/before `t`), for thumbnails.
    pub fn keyframe_near(&mut self, t: f64) -> Option<RgbaFrame> {
        self.rewind_to(t);
        self.next()
    }
}

// ───────────────────────────── audio ─────────────────────────────

/// Decodes one audio stream to interleaved stereo f32 at a fixed rate.
pub struct AudioReader {
    ictx: format::context::Input,
    stream: usize,
    tb: f64,
    start: f64,
    dec: ff::decoder::Audio,
    rs: Option<software::resampling::Context>,
    rate: u32,
    channels: usize,
    fifo: VecDeque<f32>,
    /// Interleaved samples still to drop after a seek (pre-roll).
    skip: usize,
    eof: bool,
}

impl AudioReader {
    /// `channels` = 1 (mono, for waveforms) or 2.
    pub fn open(path: &Path, stream_index: usize, at: f64, rate: u32, channels: usize) -> Result<Self> {
        let mut ictx = format::input(path)?;
        let s = ictx.stream(stream_index).ok_or_else(|| anyhow!("bad audio stream"))?;
        let tb = q(s.time_base());
        let start = stream_start(&s);
        let dec = ff::codec::Context::from_parameters(s.parameters())?.decoder().audio()?;
        if at > 0.0 {
            let ts = ((at + start) / tb) as i64;
            unsafe {
                ff::ffi::av_seek_frame(ictx.as_mut_ptr(), stream_index as i32, ts, ff::ffi::AVSEEK_FLAG_BACKWARD as i32);
            }
        }
        let mut r = Self { ictx, stream: stream_index, tb, start, dec, rs: None, rate, channels, fifo: VecDeque::new(), skip: 0, eof: false };
        r.prime(at);
        Ok(r)
    }

    /// Decode until the first frame, and set how much to drop to land exactly on `at`.
    fn prime(&mut self, at: f64) {
        let mut first_t = None;
        while first_t.is_none() && !self.eof {
            first_t = self.decode_one();
        }
        if let Some(ft) = first_t {
            let drop = ((at - ft).max(0.0) * self.rate as f64) as usize * self.channels;
            self.skip = drop;
            let d = self.skip.min(self.fifo.len());
            self.fifo.drain(..d);
            self.skip -= d;
        }
    }

    /// Decode one frame into the fifo. Returns its start time.
    fn decode_one(&mut self) -> Option<f64> {
        let mut f = frame::Audio::empty();
        loop {
            if self.dec.receive_frame(&mut f).is_ok() {
                break;
            }
            if self.eof {
                return None;
            }
            let mut sent = false;
            for (s, p) in self.ictx.packets() {
                if s.index() == self.stream {
                    let _ = self.dec.send_packet(&p);
                    sent = true;
                    break;
                }
            }
            if !sent {
                let _ = self.dec.send_eof();
                self.eof = true;
            }
        }
        let t = f.timestamp().or(f.pts()).map(|p| p as f64 * self.tb - self.start).unwrap_or(0.0);
        if f.channel_layout().is_empty() || f.channel_layout().channels() != f.channels() as i32 {
            f.set_channel_layout(ff::ChannelLayout::default(f.channels() as i32));
        }
        if self.rs.is_none() {
            let dst = if self.channels == 1 { ff::ChannelLayout::MONO } else { ff::ChannelLayout::STEREO };
            self.rs = software::resampling::Context::get(
                f.format(), f.channel_layout(), f.rate(),
                ff::format::Sample::F32(ff::format::sample::Type::Packed), dst, self.rate,
            ).ok();
        }
        let rs = self.rs.as_mut()?;
        let mut out = frame::Audio::empty();
        if rs.run(&f, &mut out).is_ok() {
            let n = out.samples() * self.channels;
            let bytes = &out.data(0)[..n * 4];
            self.fifo.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        }
        let d = self.skip.min(self.fifo.len());
        self.fifo.drain(..d);
        self.skip -= d;
        Some(t)
    }

    /// Mix `out.len()/channels` frames into `out`, multiplied by `gain`. Pads silence at EOF.
    pub fn mix_into(&mut self, out: &mut [f32], gain: f32) {
        while self.fifo.len() < out.len() && !self.eof {
            if self.decode_one().is_none() && self.eof {
                break;
            }
        }
        let n = out.len().min(self.fifo.len());
        for (o, s) in out.iter_mut().zip(self.fifo.drain(..n)) {
            *o += s * gain;
        }
    }

    /// Pull up to `n` samples (mono reader) for waveform building. Empty at EOF.
    pub fn read_chunk(&mut self, n: usize) -> Vec<f32> {
        while self.fifo.len() < n && !self.eof {
            self.decode_one();
        }
        self.fifo.drain(..n.min(self.fifo.len())).collect()
    }
}

// ───────────────────────────── background jobs ─────────────────────────────

pub const PEAKS_PER_SEC: f64 = 100.0;

/// Peak envelope (max abs per 1/100 s) of one audio stream.
pub fn build_waveform(path: &Path, stream_index: usize) -> Result<Vec<f32>> {
    const RATE: u32 = 8000;
    let mut r = AudioReader::open(path, stream_index, 0.0, RATE, 1)?;
    let per = (RATE as f64 / PEAKS_PER_SEC) as usize;
    let mut peaks = Vec::new();
    loop {
        let chunk = r.read_chunk(per * 256);
        if chunk.is_empty() {
            break;
        }
        for c in chunk.chunks(per) {
            peaks.push(c.iter().fold(0f32, |m, s| m.max(s.abs())).min(1.0));
        }
    }
    Ok(peaks)
}

pub const THUMB_H: u32 = 64;

/// Small keyframe thumbnails every `step` seconds.
pub fn build_thumbs(path: &Path, duration: f64, mut emit: impl FnMut(f64, RgbaFrame)) -> Result<()> {
    let mut d = VideoDecoder::open(path, THUMB_H * 4, THUMB_H, 2)?;
    let step = (duration / 400.0).max(2.0);
    let mut t = 0.0;
    while t < duration {
        if let Some(f) = d.keyframe_near(t) {
            emit(t, f);
        }
        t += step;
    }
    Ok(())
}
