//! Project data model: media list, one video track, N audio tracks, render settings,
//! and the pure editing operations (split, move, trim, overwrite-on-overlap).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub type ClipId = u64;

/// Shortest clip an edit is allowed to produce (seconds).
pub const MIN_CLIP: f64 = 0.02;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    /// Index into `Project::media`.
    pub media: usize,
    /// For audio clips: index among the media's audio streams. Unused for video.
    pub stream: usize,
    /// Position on the timeline (seconds).
    pub start: f64,
    /// In/out points inside the source (seconds, relative to source start).
    pub src_in: f64,
    pub src_out: f64,
    /// Clips sharing a non-zero group move/trim/select together (video + its audio).
    pub group: u64,
}

impl Clip {
    pub fn len(&self) -> f64 {
        self.src_out - self.src_in
    }
    pub fn end(&self) -> f64 {
        self.start + self.len()
    }
    /// Source time shown at timeline time `t`.
    pub fn src_at(&self, t: f64) -> f64 {
        self.src_in + (t - self.start)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioTrack {
    pub name: String,
    pub clips: Vec<Clip>,
    pub muted: bool,
    pub solo: bool,
    pub volume_db: f32,
}

impl AudioTrack {
    pub fn new(name: String) -> Self {
        Self { name, clips: Vec::new(), muted: false, solo: false, volume_db: 0.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackRef {
    Video,
    Audio(usize),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Timeline {
    pub video: Vec<Clip>,
    pub audio: Vec<AudioTrack>,
    /// Loop/render region (start, end) set with the pins on the ruler.
    pub region: Option<(f64, f64)>,
    pub next_id: u64,
}

impl Timeline {
    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn clips(&self, tr: TrackRef) -> &Vec<Clip> {
        match tr {
            TrackRef::Video => &self.video,
            TrackRef::Audio(i) => &self.audio[i].clips,
        }
    }

    pub fn clips_mut(&mut self, tr: TrackRef) -> &mut Vec<Clip> {
        match tr {
            TrackRef::Video => &mut self.video,
            TrackRef::Audio(i) => &mut self.audio[i].clips,
        }
    }

    pub fn tracks(&self) -> Vec<TrackRef> {
        let mut v = vec![TrackRef::Video];
        v.extend((0..self.audio.len()).map(TrackRef::Audio));
        v
    }

    pub fn all_clips(&self) -> impl Iterator<Item = (TrackRef, &Clip)> {
        self.video
            .iter()
            .map(|c| (TrackRef::Video, c))
            .chain(self.audio.iter().enumerate().flat_map(|(i, t)| t.clips.iter().map(move |c| (TrackRef::Audio(i), c))))
    }

    pub fn find(&self, id: ClipId) -> Option<(TrackRef, &Clip)> {
        self.all_clips().find(|(_, c)| c.id == id)
    }

    pub fn end(&self) -> f64 {
        self.all_clips().map(|(_, c)| c.end()).fold(0.0, f64::max)
    }

    /// Expand a selection to every clip sharing a group with a selected clip.
    pub fn expand_groups(&self, ids: &HashSet<ClipId>) -> HashSet<ClipId> {
        let groups: HashSet<u64> =
            self.all_clips().filter(|(_, c)| ids.contains(&c.id) && c.group != 0).map(|(_, c)| c.group).collect();
        self.all_clips().filter(|(_, c)| ids.contains(&c.id) || groups.contains(&c.group)).map(|(_, c)| c.id).collect()
    }

    pub fn sort(&mut self) {
        for tr in self.tracks() {
            self.clips_mut(tr).sort_by(|a, b| a.start.total_cmp(&b.start));
        }
    }

    /// Range covered by the render: the pinned region, or the whole timeline.
    pub fn render_range(&self) -> (f64, f64) {
        match self.region {
            Some((a, b)) if b - a > MIN_CLIP => (a, b),
            _ => (0.0, self.end()),
        }
    }

    /// Place a whole media file at `at`: one video clip + one clip per audio stream, all grouped.
    pub fn add_media(&mut self, media: usize, info: &crate::media::MediaInfo, at: f64) -> Vec<ClipId> {
        let group = self.alloc_id();
        let mut ids = Vec::new();
        if info.video.is_some() {
            let id = self.alloc_id();
            self.video.push(Clip { id, media, stream: 0, start: at, src_in: 0.0, src_out: info.duration, group });
            ids.push(id);
        }
        for (si, a) in info.audio.iter().enumerate() {
            while self.audio.len() <= si {
                let name = a.title.clone().unwrap_or_else(|| format!("Audio {}", self.audio.len() + 1));
                self.audio.push(AudioTrack::new(name));
            }
            let id = self.alloc_id();
            self.audio[si].clips.push(Clip { id, media, stream: si, start: at, src_in: 0.0, src_out: info.duration, group });
            ids.push(id);
        }
        let set: HashSet<ClipId> = ids.iter().copied().collect();
        self.overwrite_with(&set);
        ids
    }

    /// Split clips crossing `t`. If `only` is given, only those clips are split.
    /// Groups are preserved: right halves of a group form a new group.
    pub fn split_at(&mut self, t: f64, only: Option<&HashSet<ClipId>>) -> bool {
        let mut new_groups: HashMap<u64, u64> = HashMap::new();
        let mut did = false;
        for tr in self.tracks() {
            let mut add = Vec::new();
            let n = self.clips(tr).len();
            for i in 0..n {
                let c = self.clips(tr)[i].clone();
                if only.is_some_and(|s| !s.contains(&c.id)) {
                    continue;
                }
                if t > c.start + MIN_CLIP && t < c.end() - MIN_CLIP {
                    let cut = c.src_at(t);
                    let right_group = if c.group == 0 {
                        0
                    } else {
                        *new_groups.entry(c.group).or_insert_with(|| {
                            self.next_id += 1;
                            self.next_id
                        })
                    };
                    let id = self.alloc_id();
                    add.push(Clip { id, start: t, src_in: cut, group: right_group, ..c.clone() });
                    self.clips_mut(tr)[i].src_out = cut;
                    did = true;
                }
            }
            self.clips_mut(tr).extend(add);
        }
        self.sort();
        did
    }

    pub fn delete(&mut self, ids: &HashSet<ClipId>) {
        for tr in self.tracks() {
            self.clips_mut(tr).retain(|c| !ids.contains(&c.id));
        }
    }

    /// Remove every part of other clips that overlaps the clips in `winners` (same track).
    /// This is the "overwrite" behaviour of a single-layer track.
    pub fn overwrite_with(&mut self, winners: &HashSet<ClipId>) {
        for tr in self.tracks() {
            let wins: Vec<(f64, f64)> =
                self.clips(tr).iter().filter(|c| winners.contains(&c.id)).map(|c| (c.start, c.end())).collect();
            if wins.is_empty() {
                continue;
            }
            let mut out: Vec<Clip> = Vec::new();
            let mut fresh: Vec<Clip> = Vec::new();
            for c in self.clips(tr).iter() {
                if winners.contains(&c.id) {
                    out.push(c.clone());
                    continue;
                }
                // Subtract every winner interval from this clip.
                let mut pieces = vec![c.clone()];
                for &(ws, we) in &wins {
                    let mut next = Vec::new();
                    for p in pieces {
                        if we <= p.start || ws >= p.end() {
                            next.push(p);
                            continue;
                        }
                        if ws > p.start + MIN_CLIP {
                            let mut l = p.clone();
                            l.src_out = l.src_at(ws);
                            next.push(l);
                        }
                        if we < p.end() - MIN_CLIP {
                            let mut r = p.clone();
                            r.src_in = r.src_at(we);
                            r.start = we;
                            r.id = 0; // assigned below if this is an extra piece
                            next.push(r);
                        }
                    }
                    pieces = next;
                }
                let mut kept_original = false;
                for mut p in pieces {
                    if p.id == c.id && !kept_original {
                        kept_original = true;
                        out.push(p);
                    } else {
                        p.id = 0;
                        fresh.push(p);
                    }
                }
            }
            for mut p in fresh {
                p.id = self.alloc_id();
                out.push(p);
            }
            *self.clips_mut(tr) = out;
        }
        self.sort();
    }

    /// Move clips by `dt` seconds and (audio only) by `dtrack` tracks.
    pub fn move_clips(&mut self, ids: &HashSet<ClipId>, dt: f64, dtrack: i32) {
        let min_start = self.all_clips().filter(|(_, c)| ids.contains(&c.id)).map(|(_, c)| c.start).fold(f64::MAX, f64::min);
        let dt = if min_start + dt < 0.0 { -min_start } else { dt };
        for tr in self.tracks() {
            for c in self.clips_mut(tr).iter_mut().filter(|c| ids.contains(&c.id)) {
                c.start += dt;
            }
        }
        if dtrack != 0 {
            let n = self.audio.len() as i32;
            let mut moving: Vec<(usize, Clip)> = Vec::new();
            for (i, t) in self.audio.iter_mut().enumerate() {
                let (m, keep): (Vec<Clip>, Vec<Clip>) = t.clips.drain(..).partition(|c| ids.contains(&c.id));
                t.clips = keep;
                moving.extend(m.into_iter().map(|c| (i, c)));
            }
            for (i, c) in moving {
                let dst = (i as i32 + dtrack).clamp(0, n - 1) as usize;
                self.audio[dst].clips.push(c);
            }
        }
        self.sort();
    }

    /// Clamp a move delta so that no clip would start before 0.
    pub fn clamp_move(&self, ids: &HashSet<ClipId>, dt: f64) -> f64 {
        let min_start = self.all_clips().filter(|(_, c)| ids.contains(&c.id)).map(|(_, c)| c.start).fold(f64::MAX, f64::min);
        if min_start == f64::MAX { 0.0 } else { dt.max(-min_start) }
    }

    /// Trim the left edge of clips by `dt` (positive = shorter). `durs[media]` bounds the source.
    pub fn trim_left(&mut self, ids: &HashSet<ClipId>, dt: f64) {
        // Use a single delta valid for every clip so linked clips stay aligned.
        let mut lo = f64::MIN;
        let mut hi = f64::MAX;
        for (_, c) in self.all_clips().filter(|(_, c)| ids.contains(&c.id)) {
            lo = lo.max(-c.src_in).max(-c.start);
            hi = hi.min(c.len() - MIN_CLIP);
        }
        let dt = dt.clamp(lo, hi.max(lo));
        for tr in self.tracks() {
            for c in self.clips_mut(tr).iter_mut().filter(|c| ids.contains(&c.id)) {
                c.src_in += dt;
                c.start += dt;
            }
        }
    }

    pub fn trim_right(&mut self, ids: &HashSet<ClipId>, dt: f64, durs: &[f64]) {
        let mut lo = f64::MIN;
        let mut hi = f64::MAX;
        for (_, c) in self.all_clips().filter(|(_, c)| ids.contains(&c.id)) {
            lo = lo.max(-(c.len() - MIN_CLIP));
            hi = hi.min(durs.get(c.media).copied().unwrap_or(c.src_out) - c.src_out);
        }
        let dt = dt.clamp(lo, hi.max(lo));
        for tr in self.tracks() {
            for c in self.clips_mut(tr).iter_mut().filter(|c| ids.contains(&c.id)) {
                c.src_out += dt;
            }
        }
    }

    /// Remove the group link of the given clips.
    pub fn ungroup(&mut self, ids: &HashSet<ClipId>) {
        for tr in self.tracks() {
            for c in self.clips_mut(tr).iter_mut().filter(|c| ids.contains(&c.id)) {
                c.group = 0;
            }
        }
    }

    /// Link the given clips into one group.
    pub fn group(&mut self, ids: &HashSet<ClipId>) {
        let g = self.alloc_id();
        for tr in self.tracks() {
            for c in self.clips_mut(tr).iter_mut().filter(|c| ids.contains(&c.id)) {
                c.group = g;
            }
        }
    }

    /// All clip edge positions (plus 0), used for snapping.
    pub fn edges(&self, exclude: &HashSet<ClipId>) -> Vec<f64> {
        let mut v = vec![0.0];
        for (_, c) in self.all_clips().filter(|(_, c)| !exclude.contains(&c.id)) {
            v.push(c.start);
            v.push(c.end());
        }
        v
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderSettings {
    /// 0 = auto (all logical cores).
    pub threads: u32,
    pub target_mib: f64,
    pub height: u32,
    pub fps: u32,
    /// libvpx `-speed` for pass 2 (0 = slowest/best).
    pub quality_preset: u32,
    pub audio: bool,
    pub audio_kbps: u32,

    // ---- advanced ----
    pub pass1_speed: u32,
    pub ten_bit: bool,
    pub denoise: bool,
    pub denoise_params: String,
    pub aq_mode: u32,
    pub auto_alt_ref: u32,
    pub lag_in_frames: u32,
    pub keyframe_sec: f64,
    /// Extra safety margin in percent subtracted from the budget.
    pub margin_pct: f64,
    /// None = automatic, chosen from output height like the .bat.
    pub tiles: Option<(u32, u32)>,
    /// Empty = look next to the app / on PATH.
    pub ffmpeg_path: String,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            threads: 0,
            target_mib: 4.0,
            height: 720,
            fps: 60,
            quality_preset: 0,
            audio: false,
            audio_kbps: 96,
            pass1_speed: 4,
            ten_bit: true,
            denoise: true,
            denoise_params: "2:2:4:4".into(),
            aq_mode: 2,
            auto_alt_ref: 1,
            lag_in_frames: 25,
            keyframe_sec: 4.0,
            margin_pct: 0.0,
            tiles: None,
            ffmpeg_path: String::new(),
        }
    }
}

impl RenderSettings {
    pub fn effective_threads(&self) -> u32 {
        if self.threads > 0 { self.threads } else { auto_threads() }
    }

    pub fn effective_tiles(&self) -> (u32, u32) {
        self.tiles.unwrap_or(if self.height >= 1080 {
            (2, 1)
        } else if self.height >= 720 {
            (2, 0)
        } else {
            (1, 0)
        })
    }

    /// Video bitrate in ffmpeg kbps (1k = 1000 bit/s), same budget convention as the .bat
    /// (MiB * 8192 "kbits"), but with fractional duration and audio = rate * duration.
    pub fn video_kbps(&self, duration: f64) -> u32 {
        if duration <= 0.0 {
            return 0;
        }
        let total = self.target_mib * 8192.0 * (1.0 - self.margin_pct / 100.0);
        let audio = if self.audio { self.audio_kbps as f64 * duration } else { 0.0 };
        ((total - audio) / duration).floor().max(1.0) as u32
    }
}

pub fn auto_threads() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(8)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub media: Vec<PathBuf>,
    pub timeline: Timeline,
    pub settings: RenderSettings,
    /// None = automatic (next to the first source).
    pub output: Option<PathBuf>,
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let ms = (t * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u64, start: f64, len: f64) -> Clip {
        Clip { id, media: 0, stream: 0, start, src_in: 0.0, src_out: len, group: 0 }
    }

    #[test]
    fn overwrite_splits_underlying_clip() {
        let mut t = Timeline { video: vec![clip(1, 0.0, 10.0), clip(2, 4.0, 2.0)], next_id: 10, ..Default::default() };
        t.overwrite_with(&HashSet::from([2]));
        let v: Vec<(f64, f64)> = t.video.iter().map(|c| (c.start, c.end())).collect();
        assert_eq!(v, vec![(0.0, 4.0), (4.0, 6.0), (6.0, 10.0)]);
        assert_eq!(t.video[2].src_in, 6.0);
    }

    #[test]
    fn split_keeps_groups_linked() {
        let mut t = Timeline { video: vec![Clip { group: 5, ..clip(1, 0.0, 10.0) }], next_id: 10, ..Default::default() };
        t.audio.push(AudioTrack::new("a".into()));
        t.audio[0].clips.push(Clip { group: 5, ..clip(2, 0.0, 10.0) });
        assert!(t.split_at(3.0, None));
        assert_eq!(t.video.len(), 2);
        assert_eq!(t.video[1].group, t.audio[0].clips[1].group);
        assert_ne!(t.video[1].group, 5);
        assert_eq!(t.video[1].src_in, 3.0);
    }

    #[test]
    fn bitrate_matches_bat_convention() {
        let s = RenderSettings { target_mib: 4.0, audio: false, ..Default::default() };
        assert_eq!(s.video_kbps(13.0), 2520); // 32768 / 13
        let s = RenderSettings { target_mib: 4.0, audio: true, audio_kbps: 96, ..Default::default() };
        assert_eq!(s.video_kbps(49.0), (32768.0 / 49.0 - 96.0) as u32);
    }

    #[test]
    fn trim_respects_source_bounds() {
        let mut t = Timeline { video: vec![clip(1, 5.0, 10.0)], ..Default::default() };
        t.trim_left(&HashSet::from([1]), -3.0);
        assert_eq!(t.video[0].src_in, 0.0);
        t.trim_right(&HashSet::from([1]), 5.0, &[12.0]);
        assert_eq!(t.video[0].src_out, 12.0);
    }
}
