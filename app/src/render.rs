//! Final render: converts the timeline region into an ffmpeg filter graph
//! (clip segments + black gaps → concat, audio tracks → delayed + mixed) and runs the
//! same 2-pass libvpx-vp9 encode as the v3webm .bat files.

use crate::media::MediaInfo;
use crate::model::{Project, TrackRef};
use anyhow::{Result, anyhow, bail};
use parking_lot::Mutex;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const AUDIO_RATE: u32 = 48000;

pub struct RenderPlan {
    pub ffmpeg: PathBuf,
    pub pass1: Vec<String>,
    pub pass2: Vec<String>,
    pub duration: f64,
    pub output: PathBuf,
    pub out_w: u32,
    pub out_h: u32,
    pub video_kbps: u32,
    pub workdir: PathBuf,
}

/// Where ffmpeg.exe is looked up when the setting is empty.
pub fn find_ffmpeg(setting: &str) -> PathBuf {
    if !setting.trim().is_empty() {
        return PathBuf::from(setting.trim());
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let p = dir.join("ffmpeg.exe");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("ffmpeg")
}

/// Default output: next to the first source in the region, never equal to an input.
pub fn auto_output(project: &Project) -> Option<PathBuf> {
    let (r0, r1) = project.timeline.render_range();
    let first = project
        .timeline
        .all_clips()
        .filter(|(_, c)| c.end() > r0 && c.start < r1)
        .min_by(|a, b| (a.0 != TrackRef::Video, a.1.start).partial_cmp(&(b.0 != TrackRef::Video, b.1.start)).unwrap())
        .map(|(_, c)| c.media)?;
    let src = project.media.get(first)?;
    let dir = src.parent()?;
    let stem = src.file_stem()?.to_string_lossy().into_owned();
    let used = |p: &Path| project.media.iter().any(|m| m.as_path() == p);
    let mut out = dir.join(format!("{stem}.webm"));
    let mut n = 1;
    while used(&out) {
        out = dir.join(format!("{stem}_{n}.webm"));
        n += 1;
    }
    Some(out)
}

fn f6(x: f64) -> String {
    format!("{:.6}", x.max(0.0))
}

struct Graph {
    inputs: Vec<String>,
    graph: String,
}

pub fn track_gains(project: &Project) -> Vec<f32> {
    let tl = &project.timeline;
    let any_solo = tl.audio.iter().any(|t| t.solo);
    tl.audio
        .iter()
        .map(|t| {
            if t.muted || (any_solo && !t.solo) { 0.0 } else { 10f32.powf(t.volume_db / 20.0) }
        })
        .collect()
}

fn build_graph(project: &Project, media: &[MediaInfo], w: u32, h: u32, with_audio: bool) -> Result<Graph> {
    let s = &project.settings;
    let tl = &project.timeline;
    let (r0, r1) = tl.render_range();
    let dur = r1 - r0;
    let pix = if s.ten_bit { "yuv420p10le" } else { "yuv420p" };
    let fps = s.fps;
    let mut inputs: Vec<String> = Vec::new();
    let mut g = String::new();
    let mut n_in = 0usize;
    let mut add_input = |inputs: &mut Vec<String>, path: &Path, src: f64, len: f64| -> usize {
        inputs.extend(["-ss".into(), f6(src), "-t".into(), f6(len), "-i".into(), path.to_string_lossy().into_owned()]);
        n_in += 1;
        n_in - 1
    };

    // ---- video: clips and black gaps, in order ----
    let mut clips: Vec<_> = tl.video.iter().filter(|c| c.end() > r0 + 1e-4 && c.start < r1 - 1e-4).collect();
    clips.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut t = r0;
    let mut seg = 0usize;
    let gap = |g: &mut String, seg: usize, d: f64| {
        let _ = write!(g, "color=c=black:s={w}x{h}:r={fps}:d={},format={pix},setsar=1[v{seg}];", f6(d));
    };
    for c in clips {
        let a = c.start.max(r0);
        let b = c.end().min(r1);
        if b - a < 1e-3 {
            continue;
        }
        if a > t + 1e-3 {
            gap(&mut g, seg, a - t);
            seg += 1;
        }
        let path = &media.get(c.media).ok_or_else(|| anyhow!("missing media"))?.path;
        let i = add_input(&mut inputs, path, c.src_at(a), b - a);
        let _ = write!(
            g,
            "[{i}:v:0]fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,format={pix},trim=duration={},setpts=PTS-STARTPTS[v{seg}];",
            f6(b - a)
        );
        seg += 1;
        t = b;
    }
    if r1 > t + 1e-3 {
        gap(&mut g, seg, r1 - t);
        seg += 1;
    }
    for k in 0..seg {
        let _ = write!(g, "[v{k}]");
    }
    let _ = write!(g, "concat=n={seg}:v=1:a=0[vc];");
    if s.denoise {
        let _ = write!(g, "[vc]hqdn3d={}[vout]", s.denoise_params.trim());
    } else {
        g.push_str("[vc]null[vout]");
    }

    // ---- audio: every audible clip delayed to its position, mixed over silence ----
    if with_audio {
        let gains = track_gains(project);
        let _ = write!(g, ";anullsrc=r={AUDIO_RATE}:cl=stereo,atrim=duration={}[abase]", f6(dur));
        let mut labels = vec!["[abase]".to_string()];
        for (ti, track) in tl.audio.iter().enumerate() {
            if gains[ti] <= 0.0 {
                continue;
            }
            for c in &track.clips {
                let a = c.start.max(r0);
                let b = c.end().min(r1);
                if b - a < 1e-3 {
                    continue;
                }
                let m = media.get(c.media).ok_or_else(|| anyhow!("missing media"))?;
                let Some(ai) = m.audio.get(c.stream) else { continue };
                let i = add_input(&mut inputs, &m.path, c.src_at(a), b - a);
                let k = labels.len();
                let delay_ms = ((a - r0) * 1000.0).round() as i64;
                let _ = write!(
                    g,
                    ";[{i}:{}]aresample={AUDIO_RATE},aformat=sample_fmts=fltp:channel_layouts=stereo,volume={:.4},adelay={delay_ms}:all=1[a{k}]",
                    ai.index, gains[ti]
                );
                labels.push(format!("[a{k}]"));
            }
        }
        let _ = write!(g, ";{}amix=inputs={}:duration=first:normalize=0:dropout_transition=0[aout]", labels.concat(), labels.len());
    }
    Ok(Graph { inputs, graph: g })
}

pub fn plan(project: &Project, media: &[MediaInfo]) -> Result<RenderPlan> {
    let s = &project.settings;
    let tl = &project.timeline;
    let (r0, r1) = tl.render_range();
    let dur = r1 - r0;
    if dur < 0.1 {
        bail!("Nothing to render: the timeline/region is empty.");
    }
    // Output frame aspect = first video clip in the region (mixed sources get letterboxed).
    let aspect = tl
        .video
        .iter()
        .filter(|c| c.end() > r0 && c.start < r1)
        .min_by(|a, b| a.start.total_cmp(&b.start))
        .and_then(|c| media.get(c.media)?.video.as_ref())
        .map(|v| v.width as f64 / v.height.max(1) as f64)
        .unwrap_or(16.0 / 9.0);
    let h = (s.height.max(16) / 2) * 2;
    let w = (((h as f64 * aspect) / 2.0).round() as u32 * 2).max(16);
    let output = match &project.output {
        Some(p) => p.clone(),
        None => auto_output(project).ok_or_else(|| anyhow!("No output path"))?,
    };
    if project.media.iter().any(|m| m == &output) {
        bail!("Output file would overwrite one of the sources.");
    }
    let kbps = s.video_kbps(dur);
    let workdir = std::env::temp_dir().join(format!("webm_studio_{}_{}", std::process::id(), rand_seed()));
    std::fs::create_dir_all(&workdir)?;
    let passlog = workdir.join("vp9_pass").to_string_lossy().into_owned();
    let (tc, tr) = s.effective_tiles();
    let threads = s.effective_threads();
    let keyint = ((s.fps as f64 * s.keyframe_sec).round() as u32).max(1);

    let common = |speed: u32, pass: u32| -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-c:v", "libvpx-vp9", "-pass", &pass.to_string(), "-passlogfile", &passlog,
            "-threads", &threads.to_string(), "-quality", "good", "-speed", &speed.to_string(),
        ].into_iter().map(String::from).collect();
        if s.ten_bit {
            a.extend(["-pix_fmt", "yuv420p10le", "-profile:v", "2"].map(String::from));
        } else {
            a.extend(["-pix_fmt", "yuv420p", "-profile:v", "0"].map(String::from));
        }
        a.extend([
            "-b:v".into(), format!("{kbps}k"),
            "-auto-alt-ref".into(), s.auto_alt_ref.to_string(),
            "-lag-in-frames".into(), s.lag_in_frames.to_string(),
            "-g".into(), keyint.to_string(),
            "-row-mt".into(), "1".into(),
            "-tile-columns".into(), tc.to_string(),
            "-tile-rows".into(), tr.to_string(),
            "-aq-mode".into(), s.aq_mode.to_string(),
            "-static-thresh".into(), "0".into(),
            "-map_metadata".into(), "-1".into(),
            "-map_chapters".into(), "-1".into(),
            "-t".into(), f6(dur),
        ]);
        a
    };
    let head = || -> Vec<String> {
        ["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:1", "-nostats"].map(String::from).to_vec()
    };

    let mut pass1 = head();
    let g1 = build_graph(project, media, w, h, false)?;
    let f1 = workdir.join("graph1.txt");
    std::fs::write(&f1, &g1.graph)?;
    pass1.extend(g1.inputs);
    pass1.extend(["-/filter_complex".into(), f1.to_string_lossy().into_owned(), "-map".into(), "[vout]".into(), "-an".into()]);
    pass1.extend(common(s.pass1_speed, 1));
    pass1.extend(["-f", "null", "NUL"].map(String::from));

    let mut pass2 = head();
    let g2 = build_graph(project, media, w, h, s.audio)?;
    let f2 = workdir.join("graph2.txt");
    std::fs::write(&f2, &g2.graph)?;
    pass2.extend(g2.inputs);
    pass2.extend(["-/filter_complex".into(), f2.to_string_lossy().into_owned(), "-map".into(), "[vout]".into()]);
    if s.audio {
        pass2.extend(["-map".into(), "[aout]".into(), "-c:a".into(), "libopus".into(), "-b:a".into(), format!("{}k", s.audio_kbps)]);
    } else {
        pass2.push("-an".into());
    }
    pass2.extend(common(s.quality_preset, 2));
    pass2.extend(["-f".into(), "webm".into(), output.to_string_lossy().into_owned()]);

    Ok(RenderPlan { ffmpeg: find_ffmpeg(&s.ffmpeg_path), pass1, pass2, duration: dur, output, out_w: w, out_h: h, video_kbps: kbps, workdir })
}

fn rand_seed() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Pass(u32),
    Done { bytes: u64, secs: f64 },
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    /// Overall progress 0..1.
    pub progress: f32,
    pub started: Instant,
    pub output: PathBuf,
    pub target_bytes: u64,
}

pub struct RenderJob {
    pub status: Arc<Mutex<Status>>,
    cancel: Arc<AtomicBool>,
}

impl RenderJob {
    pub fn start(plan: RenderPlan, target_mib: f64, repaint: impl Fn() + Send + 'static) -> Self {
        let status = Arc::new(Mutex::new(Status {
            phase: Phase::Pass(1),
            progress: 0.0,
            started: Instant::now(),
            output: plan.output.clone(),
            target_bytes: (target_mib * 1024.0 * 1024.0) as u64,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let (st, cc) = (status.clone(), cancel.clone());
        std::thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let res = run_pass(&plan, 1, &st, &cc, &repaint).and_then(|_| run_pass(&plan, 2, &st, &cc, &repaint));
                let phase = match res {
                    _ if cc.load(Ordering::SeqCst) => {
                        let _ = std::fs::remove_file(&plan.output);
                        Phase::Cancelled
                    }
                    Ok(()) => Phase::Done {
                        bytes: std::fs::metadata(&plan.output).map(|m| m.len()).unwrap_or(0),
                        secs: st.lock().started.elapsed().as_secs_f64(),
                    },
                    Err(e) => Phase::Failed(e.to_string()),
                };
                let _ = std::fs::remove_dir_all(&plan.workdir);
                let mut s = st.lock();
                s.phase = phase;
                s.progress = 1.0;
                drop(s);
                repaint();
            })
            .unwrap();
        Self { status, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn running(&self) -> bool {
        matches!(self.status.lock().phase, Phase::Pass(_))
    }
}

fn run_pass(plan: &RenderPlan, pass: u32, st: &Mutex<Status>, cancel: &AtomicBool, repaint: &dyn Fn()) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    st.lock().phase = Phase::Pass(pass);
    let args = if pass == 1 { &plan.pass1 } else { &plan.pass2 };
    let mut child = Command::new(&plan.ffmpeg)
        .args(args)
        .current_dir(&plan.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| anyhow!("cannot start ffmpeg ({}): {e}", plan.ffmpeg.display()))?;
    let mut stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let (w0, w1) = if pass == 1 { (0.0, 0.3) } else { (0.3, 1.0) };
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            break;
        }
        let Ok(line) = line else { break };
        if let Some(v) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
            let frac = (v / 1e6 / plan.duration).clamp(0.0, 1.0);
            st.lock().progress = (w0 + (w1 - w0) * frac) as f32;
            repaint();
        }
    }
    let code = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if cancel.load(Ordering::SeqCst) {
        bail!("cancelled");
    }
    if !code.success() {
        let tail: Vec<&str> = err.lines().rev().take(8).collect();
        bail!("ffmpeg pass {pass} failed:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    Ok(())
}

/// Shell-quoted command lines, for the "copy command" button.
pub fn describe(plan: &RenderPlan) -> String {
    let q = |a: &String| if a.contains(' ') || a.contains('[') { format!("\"{a}\"") } else { a.clone() };
    let exe = format!("\"{}\"", plan.ffmpeg.display());
    format!(
        "{exe} {}\n\n{exe} {}",
        plan.pass1.iter().map(q).collect::<Vec<_>>().join(" "),
        plan.pass2.iter().map(q).collect::<Vec<_>>().join(" ")
    )
}
