//! Integration checks against a real recording. Run with:
//!   set WS_SAMPLE=path\to\clip.mp4 && cargo test -- --ignored --nocapture

use crate::media::{self, AudioReader, VideoDecoder};
use crate::model::{Clip, Project, RenderSettings};
use crate::render::{self, Phase, RenderJob};
use std::path::PathBuf;
use std::time::Instant;

fn sample() -> Option<PathBuf> {
    std::env::var_os("WS_SAMPLE").map(PathBuf::from)
}

#[test]
#[ignore]
fn decode_and_seek() {
    let Some(p) = sample() else { return };
    ffmpeg_next::init().unwrap();
    let info = media::probe(&p).unwrap();
    println!("probe: {:.3}s video={:?} audio={:?}", info.duration, info.video, info.audio);
    for th in [1usize, 4, 16] {
        let mut d = VideoDecoder::open(&p, 1280, 720, th).unwrap();
        if th == 1 { println!("hw decode: {}", d.hw); }
        let s = Instant::now();
        for t in [5.0, 2.5, 10.0, 9.0, 3.3] {
            d.frame_at(t).unwrap();
        }
        let k = Instant::now();
        for t in [5.0, 2.5, 10.0, 9.0, 3.3] {
            d.keyframe_near(t).unwrap();
        }
        println!("threads={th}: 5 exact seeks {:?}, 5 keyframe seeks {:?}", s.elapsed(), k.elapsed());
    }
    let mut d = VideoDecoder::open(&p, 1280, 720, 16).unwrap();
    for t in [0.0, 5.0, 2.5, 2.52, 10.0, 9.0, info.duration - 0.5] {
        let s = Instant::now();
        let f = d.frame_at(t).expect("frame");
        println!("frame_at({t:.3}) -> t={:.4} {}x{} in {:?}", f.t, f.w, f.h, s.elapsed());
        assert!((f.t - t).abs() <= d.frame_dur() + 1e-3, "inaccurate seek");
    }
    d.frame_at(1.0).unwrap();
    let s = Instant::now();
    for _ in 0..120 {
        d.next().unwrap();
    }
    println!("sequential 120 frames: {:?}", s.elapsed());

    for a in &info.audio {
        let mut r = AudioReader::open(&p, a.index, 3.0, 48000, 2).unwrap();
        let mut buf = vec![0f32; 48000 * 2];
        r.mix_into(&mut buf, 1.0);
        let peak = buf.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!("audio stream {} ({:?}): peak over 1s = {peak:.3}", a.index, a.title);
        let s = Instant::now();
        let w = media::build_waveform(&p, a.index).unwrap();
        println!("  waveform {} peaks in {:?}", w.len(), s.elapsed());
    }
}

#[test]
#[ignore]
fn render_with_gap_and_audio() {
    let Some(p) = sample() else { return };
    ffmpeg_next::init().unwrap();
    let info = media::probe(&p).unwrap();
    let out = std::env::temp_dir().join("webm_studio_selftest.webm");
    let mut proj = Project {
        media: vec![p.clone()],
        settings: RenderSettings { target_mib: 1.0, height: 360, fps: 30, quality_preset: 4, audio: true, ..Default::default() },
        output: Some(out.clone()),
        ..Default::default()
    };
    // Clip A: source 2..4s at 0s; gap 1s; clip B: source 10..12s at 3s. Audio linked on all tracks.
    let tl = &mut proj.timeline;
    let mut id = 0;
    let mut mk = |start: f64, src_in: f64, stream: usize| {
        id += 1;
        Clip { id, media: 0, stream, start, src_in, src_out: src_in + 2.0, group: 0 }
    };
    tl.video.push(mk(0.0, 2.0, 0));
    tl.video.push(mk(3.0, 10.0, 0));
    for (si, _) in info.audio.iter().enumerate() {
        tl.audio.push(crate::model::AudioTrack::new(format!("A{si}")));
        let a = mk(0.0, 2.0, si);
        let b = mk(3.0, 10.0, si);
        tl.audio[si].clips.extend([a, b]);
    }
    let plan = render::plan(&proj, &[info]).unwrap();
    println!("{}", render::describe(&plan));
    let job = RenderJob::start(plan, 1.0, || {});
    while job.running() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let st = job.status.lock().clone();
    println!("phase: {:?}", st.phase);
    assert!(matches!(st.phase, Phase::Done { .. }));
    let o = media::probe(&out).unwrap();
    println!("output: {:.3}s video={:?} audio={}", o.duration, o.video, o.audio.len());
    assert!((o.duration - 5.0).abs() < 0.1, "duration {}", o.duration);
    assert_eq!(o.audio.len(), 1);
    // The gap (2..3s) must be black.
    let mut d = VideoDecoder::open(&out, 64, 64, 1).unwrap();
    let f = d.frame_at(2.5).unwrap();
    let avg = f.pixels.chunks(4).map(|p| p[0] as u32 + p[1] as u32 + p[2] as u32).sum::<u32>() / (f.w * f.h) as u32;
    println!("gap frame avg luma-ish = {avg}");
    assert!(avg < 20);
}
