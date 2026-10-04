//! Vegas-style timeline: region bar (pins), ruler, one video lane, N audio lanes.
//! Drags are recomputed from a snapshot taken at drag start, so they are fully reversible
//! until release, and one undo step is recorded per drag.

use crate::app::{App, BarDrag, BarPart, Drag, DragKind};
use crate::media::PEAKS_PER_SEC;
use crate::model::{Clip, ClipId, TrackRef, fmt_time};
use crate::theme;
use egui::{Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::collections::HashSet;

const HEADER_W: f32 = 190.0;
const REGION_H: f32 = 24.0;
/// Grab distance for region pins.
const PIN_PX: f32 = 10.0;
/// A new region only replaces the old one once the drag spans this many pixels.
const REGION_MIN_PX: f32 = 4.0;
const RULER_H: f32 = 24.0;
const VIDEO_H: f32 = 74.0;
const AUDIO_H: f32 = 58.0;
const EDGE_PX: f32 = 6.0;
const SNAP_PX: f32 = 8.0;
/// Horizontal scroll bar under the tracks.
const BAR_H: f32 = 16.0;
/// Grab width of the scroll-bar thumb ends (zoom handles).
const BAR_EDGE_PX: f32 = 7.0;
const MIN_PPS: f32 = 0.5;
const MAX_PPS: f32 = 4000.0;

struct Layout {
    lanes_x: f32,
    lanes_right: f32,
    region: Rect,
    ruler: Rect,
    lanes: Vec<(TrackRef, Rect)>,
    pps: f64,
    scroll: f64,
}

impl Layout {
    fn x(&self, t: f64) -> f32 {
        self.lanes_x + ((t - self.scroll) * self.pps) as f32
    }
    fn t(&self, x: f32) -> f64 {
        self.scroll + (x - self.lanes_x) as f64 / self.pps
    }
    fn lane_at(&self, p: Pos2) -> Option<(TrackRef, Rect)> {
        self.lanes.iter().find(|(_, r)| r.y_range().contains(p.y)).copied()
    }
}

enum Hit {
    ClipBody(ClipId),
    ClipLeft(ClipId),
    ClipRight(ClipId),
}

impl App {
    /// Draws the timeline; returns the timeline time under the pointer (for file drops).
    pub fn timeline_ui(&mut self, ui: &mut Ui) -> Option<f64> {
        let avail = ui.available_rect_before_wrap();
        let full = Rect::from_min_max(avail.min, pos2(avail.right(), avail.bottom() - BAR_H));
        let bar = Rect::from_min_max(pos2(avail.left() + HEADER_W, full.bottom()), avail.max);
        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        let painter = ui.painter_at(full);

        let lanes_x = full.left() + HEADER_W;
        let lanes_w = (full.right() - lanes_x).max(50.0);
        if self.view.fit_pending {
            let end = self.project.timeline.end().max(1.0);
            self.view.px_per_sec = (((lanes_w - 40.0).max(10.0) as f64 / end) as f32).clamp(MIN_PPS, MAX_PPS);
            self.view.scroll = 0.0;
            self.view.fit_pending = false;
        }
        let mut lanes = Vec::new();
        let mut y = full.top() + REGION_H + RULER_H;
        lanes.push((TrackRef::Video, Rect::from_min_max(pos2(lanes_x, y), pos2(full.right(), y + VIDEO_H))));
        y += VIDEO_H;
        for i in 0..self.project.timeline.audio.len() {
            lanes.push((TrackRef::Audio(i), Rect::from_min_max(pos2(lanes_x, y), pos2(full.right(), y + AUDIO_H))));
            y += AUDIO_H;
        }
        let lay = Layout {
            lanes_x,
            lanes_right: full.right(),
            region: Rect::from_min_max(pos2(lanes_x, full.top()), pos2(full.right(), full.top() + REGION_H)),
            ruler: Rect::from_min_max(pos2(lanes_x, full.top() + REGION_H), pos2(full.right(), full.top() + REGION_H + RULER_H)),
            lanes,
            pps: self.view.px_per_sec as f64,
            scroll: self.view.scroll,
        };

        self.timeline_input(ui, &resp, &lay, full);
        self.scroll_bar_ui(ui, bar, lanes_w);
        self.follow_cursor(lanes_w);
        // Layout may have changed scroll/zoom; recompute transforms for drawing.
        let lay = Layout { pps: self.view.px_per_sec as f64, scroll: self.view.scroll, ..lay };
        self.draw_timeline(ui, &painter, &lay, full);
        self.track_headers(ui, &lay, full);

        resp.hover_pos().or_else(|| ui.ctx().pointer_hover_pos()).filter(|p| full.contains(*p) && p.x > lanes_x).map(|p| lay.t(p.x).max(0.0))
    }

    /// Seconds of timeline the scroll bar spans: the content, or further if the view is scrolled past it.
    fn scroll_extent(&self, visible: f64) -> f64 {
        let mut end = self.project.timeline.end().max(self.cursor);
        if let Some((_, b)) = self.project.timeline.region {
            end = end.max(b);
        }
        end.max(self.view.scroll + visible).max(visible)
    }

    /// Keep the cursor on screen during playback (Vegas-style page scroll) and after keyboard jumps.
    /// Never while the mouse is dragging, so nothing slides under the pointer.
    fn follow_cursor(&mut self, lanes_w: f32) {
        if self.drag.is_some() || self.view.bar_drag.is_some() {
            return;
        }
        let pps = self.view.px_per_sec as f64;
        let visible = lanes_w as f64 / pps;
        let margin = 24.0 / pps;
        let (left, right) = (self.view.scroll, self.view.scroll + visible);
        let off = self.cursor < left || self.cursor > right - margin;
        if self.playing && off {
            // Page so the cursor lands near the left edge, like Vegas.
            self.view.scroll = (self.cursor - visible * 0.05).max(0.0);
        } else if self.view.reveal && off {
            self.view.scroll = (self.cursor - visible * 0.5).max(0.0);
        }
        self.view.reveal = false;
    }

    fn scroll_bar_ui(&mut self, ui: &mut Ui, bar: Rect, lanes_w: f32) {
        let resp = ui.allocate_rect(bar, Sense::click_and_drag());
        let w = bar.width().max(1.0) as f64;
        let visible = lanes_w as f64 / self.view.px_per_sec as f64;
        // While dragging, the scale is frozen so the thumb stays under the mouse.
        let extent = match self.view.bar_drag {
            Some(d) => d.secs_per_px * w,
            None => self.scroll_extent(visible),
        };
        let thumb_of = |scroll: f64, vis: f64| {
            let tw = ((vis / extent * w) as f32).clamp(2.0 * BAR_EDGE_PX + 6.0, bar.width());
            let x0 = (bar.left() + (scroll / extent * w) as f32).min(bar.right() - tw);
            Rect::from_min_max(pos2(x0, bar.top() + 2.0), pos2(x0 + tw, bar.bottom() - 2.0))
        };
        let thumb = thumb_of(self.view.scroll, visible);
        let part_at = |x: f32| {
            if (x - thumb.left()).abs() <= BAR_EDGE_PX {
                Some(BarPart::LeftEdge)
            } else if (x - thumb.right()).abs() <= BAR_EDGE_PX {
                Some(BarPart::RightEdge)
            } else if thumb.x_range().contains(x) {
                Some(BarPart::Thumb)
            } else {
                None
            }
        };

        if let Some(h) = resp.hover_pos()
            && self.view.bar_drag.is_none()
        {
            match part_at(h.x) {
                Some(BarPart::LeftEdge | BarPart::RightEdge) => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
                Some(BarPart::Thumb) => ui.ctx().set_cursor_icon(CursorIcon::Grab),
                None => {}
            }
        }

        if resp.double_clicked() {
            self.view.fit_pending = true;
        } else if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
            && part_at(p.x).is_none()
        {
            // Click in the trough: page towards the click.
            let page = visible * 0.9;
            self.view.scroll = if p.x < thumb.left() { (self.view.scroll - page).max(0.0) } else { self.view.scroll + page };
        }

        if resp.drag_started()
            && let Some(o) = ui.ctx().input(|i| i.pointer.press_origin())
        {
            let part = part_at(o.x).unwrap_or_else(|| {
                // Grabbing the trough: centre the thumb there, then drag it.
                self.view.scroll = (((o.x - bar.left()) as f64 / w) * extent - visible * 0.5).max(0.0);
                BarPart::Thumb
            });
            self.view.bar_drag = Some(BarDrag { part, press_x: o.x, scroll: self.view.scroll, visible, secs_per_px: extent / w });
        }
        if let Some(d) = self.view.bar_drag {
            if resp.dragged()
                && let Some(p) = resp.interact_pointer_pos()
            {
                let dt = (p.x - d.press_x) as f64 * d.secs_per_px;
                let min_vis = lanes_w as f64 / MAX_PPS as f64;
                let max_vis = lanes_w as f64 / MIN_PPS as f64;
                match d.part {
                    BarPart::Thumb => self.view.scroll = (d.scroll + dt).max(0.0),
                    BarPart::RightEdge => {
                        // Left end stays put; dragging right shows more time (zoom out).
                        let vis = (d.visible + dt).clamp(min_vis, max_vis);
                        self.view.px_per_sec = (lanes_w as f64 / vis) as f32;
                    }
                    BarPart::LeftEdge => {
                        // Right end stays put.
                        let right = d.scroll + d.visible;
                        let scroll = (d.scroll + dt).clamp((right - max_vis).max(0.0), (right - min_vis).max(0.0));
                        self.view.scroll = scroll;
                        self.view.px_per_sec = (lanes_w as f64 / (right - scroll)) as f32;
                    }
                }
            }
            if !resp.dragged() && !ui.ctx().input(|i| i.pointer.any_down()) {
                self.view.bar_drag = None;
            }
        }

        // draw
        let strip = Rect::from_min_max(pos2(bar.left() - HEADER_W, bar.top()), bar.max);
        let painter = ui.painter_at(strip);
        painter.rect_filled(strip, 0.0, theme::HEADER_BG);
        painter.rect_filled(bar, 0.0, theme::RULER_BG);
        let visible = lanes_w as f64 / self.view.px_per_sec as f64;
        let thumb = thumb_of(self.view.scroll, visible);
        let active = self.view.bar_drag.is_some() || resp.hovered();
        painter.rect_filled(thumb, 3.0, if active { Color32::from_rgb(0x6a, 0x6e, 0x78) } else { Color32::from_rgb(0x52, 0x55, 0x5d) });
        for x in [thumb.left() + 3.0, thumb.right() - 3.0] {
            painter.line_segment([pos2(x, thumb.top() + 3.0), pos2(x, thumb.bottom() - 3.0)], Stroke::new(2.0, theme::TEXT));
        }
        // Where the region and the cursor are, at a glance.
        let tx = |t: f64| bar.left() + (t / extent * w) as f32;
        if let Some((a, b)) = self.project.timeline.region {
            painter.rect_filled(Rect::from_min_max(pos2(tx(a), bar.bottom() - 3.0), pos2(tx(b).max(tx(a) + 1.0), bar.bottom())), 0.0, theme::REGION);
        }
        let cx = tx(self.cursor);
        painter.line_segment([pos2(cx, bar.top()), pos2(cx, bar.bottom())], Stroke::new(1.0, theme::CURSOR));
    }

    fn hit_test(&self, lay: &Layout, p: Pos2) -> Option<Hit> {
        let (tr, _) = lay.lane_at(p)?;
        let clips = self.project.timeline.clips(tr);
        // Prefer edges of any clip within EDGE_PX, then bodies.
        for c in clips.iter() {
            let (x0, x1) = (lay.x(c.start), lay.x(c.end()));
            let edge = EDGE_PX.min((x1 - x0) / 3.0);
            if (p.x - x0).abs() <= edge {
                return Some(Hit::ClipLeft(c.id));
            }
            if (p.x - x1).abs() <= edge {
                return Some(Hit::ClipRight(c.id));
            }
        }
        clips.iter().find(|c| p.x >= lay.x(c.start) && p.x < lay.x(c.end())).map(|c| Hit::ClipBody(c.id))
    }

    fn select_click(&mut self, id: ClipId, ctrl: bool) {
        let group = self.grouped(&HashSet::from([id]));
        if ctrl {
            if self.selection.contains(&id) {
                self.selection.retain(|x| !group.contains(x));
            } else {
                self.selection.extend(group);
            }
        } else if !self.selection.contains(&id) {
            self.selection = group;
        }
    }

    /// Snap `t` to the nearest candidate within SNAP_PX; returns the offset applied.
    fn snap_offset(&self, lay: &Layout, times: &[f64], exclude: &HashSet<ClipId>) -> Option<f64> {
        if !self.snapping {
            return None;
        }
        let mut cands = self.project.timeline.edges(exclude);
        cands.push(self.cursor);
        // Region pins attract clips, but never themselves while the region is being dragged.
        let region_drag = matches!(
            self.drag.as_ref().map(|d| &d.kind),
            Some(DragKind::RegionNew { .. } | DragKind::RegionStart | DragKind::RegionEnd | DragKind::RegionMove { .. })
        );
        if !region_drag && let Some((a, b)) = self.project.timeline.region {
            cands.extend([a, b]);
        }
        let thr = SNAP_PX as f64 / lay.pps;
        let mut best: Option<f64> = None;
        for &t in times {
            for &c in &cands {
                let d = c - t;
                if d.abs() <= thr && best.is_none_or(|b| d.abs() < b.abs()) {
                    best = Some(d);
                }
            }
        }
        best
    }

    fn timeline_input(&mut self, ui: &Ui, resp: &egui::Response, lay: &Layout, full: Rect) {
        let ctx = ui.ctx().clone();
        let (mods, hover) = ctx.input(|i| (i.modifiers, i.pointer.hover_pos()));
        let ctrl = mods.command;

        // ---- wheel: zoom around the mouse, shift/horizontal = scroll ----
        if let Some(h) = hover.filter(|p| full.contains(*p)) {
            let delta = ctx.input(|i| {
                i.raw.events.iter().fold(egui::Vec2::ZERO, |acc, e| match e {
                    egui::Event::MouseWheel { unit, delta, .. } => {
                        acc + *delta * match unit {
                            egui::MouseWheelUnit::Point => 1.0,
                            egui::MouseWheelUnit::Line => 40.0,
                            egui::MouseWheelUnit::Page => 400.0,
                        }
                    }
                    _ => acc,
                })
            });
            let anchor = lay.t(h.x.max(lay.lanes_x));
            if mods.shift || delta.x.abs() > delta.y.abs() {
                let d = if delta.x != 0.0 { delta.x } else { delta.y };
                self.view.scroll = (self.view.scroll - d as f64 / lay.pps).max(0.0);
            } else if delta.y != 0.0 {
                self.zoom_at(1.0025f32.powf(delta.y), Some(anchor));
            }
        }

        // ---- hover cursor icons ----
        if let Some(h) = hover.filter(|p| full.contains(*p) && self.drag.is_none()) {
            if lay.region.contains(h) {
                match self.region_hit(lay, h.x) {
                    Some(DragKind::RegionStart | DragKind::RegionEnd) => ctx.set_cursor_icon(CursorIcon::ResizeHorizontal),
                    Some(DragKind::RegionMove { .. }) => ctx.set_cursor_icon(CursorIcon::Grab),
                    _ => {}
                }
            } else if matches!(self.hit_test(lay, h), Some(Hit::ClipLeft(_) | Hit::ClipRight(_))) {
                ctx.set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
        }

        // ---- double clicks ----
        if resp.double_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            if lay.region.contains(p) || lay.ruler.contains(p) {
                // nothing: double-clicking the bar must never lose the region
            } else if let Some(Hit::ClipBody(id) | Hit::ClipLeft(id) | Hit::ClipRight(id)) = self.hit_test(lay, p)
                && let Some((_, c)) = self.project.timeline.find(id)
            {
                self.project.timeline.region = Some((c.start, c.end()));
            }
            return;
        }

        // ---- clicks ----
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
            && p.x > lay.lanes_x
        {
            if lay.ruler.contains(p) || lay.region.contains(p) {
                self.seek(lay.t(p.x).max(0.0));
            } else if let Some(hit) = self.hit_test(lay, p) {
                let (Hit::ClipBody(id) | Hit::ClipLeft(id) | Hit::ClipRight(id)) = hit;
                if ctrl {
                    self.select_click(id, true);
                } else {
                    self.selection = self.grouped(&HashSet::from([id]));
                }
                self.seek(self.frame_snap(lay.t(p.x).max(0.0)));
            } else {
                self.selection.clear();
                self.seek(self.frame_snap(lay.t(p.x).max(0.0)));
            }
        }
        if resp.clicked_by(egui::PointerButton::Secondary) {
            self.selection.clear();
        }

        // ---- drag start ----
        if resp.drag_started()
            && let Some(origin) = ctx.input(|i| i.pointer.press_origin())
        {
            let t0 = lay.t(origin.x);
            let middle = resp.dragged_by(egui::PointerButton::Middle);
            let kind = if middle {
                Some(DragKind::Pan { scroll: self.view.scroll })
            } else if origin.x < lay.lanes_x {
                None
            } else if lay.region.contains(origin) {
                Some(self.region_hit(lay, origin.x).unwrap_or(DragKind::RegionNew {
                    anchor: self.snap_point(lay, t0.max(0.0)),
                    prev: self.project.timeline.region,
                }))
            } else if lay.ruler.contains(origin) {
                Some(DragKind::Scrub)
            } else {
                match self.hit_test(lay, origin) {
                    Some(hit) => {
                        let (Hit::ClipBody(id) | Hit::ClipLeft(id) | Hit::ClipRight(id)) = hit;
                        self.select_click(id, ctrl);
                        if !self.selection.contains(&id) {
                            self.selection = self.grouped(&HashSet::from([id]));
                        }
                        match hit {
                            Hit::ClipBody(_) => {
                                let audio_lane = matches!(lay.lane_at(origin), Some((TrackRef::Audio(_), _)));
                                Some(DragKind::Move { ids: self.grouped(&self.selection), audio_lane })
                            }
                            // Trims apply to the clicked clip and its linked clips only.
                            Hit::ClipLeft(_) => Some(DragKind::TrimLeft { ids: self.grouped(&HashSet::from([id])) }),
                            Hit::ClipRight(_) => Some(DragKind::TrimRight { ids: self.grouped(&HashSet::from([id])) }),
                        }
                    }
                    // Like Vegas: dragging empty track space makes a time selection (= region).
                    None => Some(DragKind::RegionNew { anchor: self.snap_point(lay, t0.max(0.0)), prev: self.project.timeline.region }),
                }
            };
            if let Some(kind) = kind {
                self.drag = Some(Drag { kind, origin_t: t0, origin_y: origin.y, orig: self.project.timeline.clone(), changed: false });
            }
        }

        // ---- dragging ----
        if let (Some(drag), Some(p)) = (self.drag.clone(), resp.interact_pointer_pos().or(hover)) {
            if resp.dragged() {
                let t = lay.t(p.x);
                let dt = t - drag.origin_t;
                match &drag.kind {
                    DragKind::Pan { scroll } => {
                        self.view.scroll = (scroll - (p.x - ctx.input(|i| i.pointer.press_origin()).map_or(p.x, |o| o.x)) as f64 / lay.pps).max(0.0);
                    }
                    DragKind::Scrub => {
                        let t = self.snap_point(lay, t.max(0.0));
                        if (t - self.cursor).abs() > 1e-9 {
                            self.scrub(t);
                        }
                    }
                    DragKind::RegionNew { anchor, prev } => {
                        let t = self.snap_point(lay, t.max(0.0));
                        let (a, b) = if t < *anchor { (t, *anchor) } else { (*anchor, t) };
                        let wide = (p.x - lay.x(*anchor)).abs() >= REGION_MIN_PX && b - a > 1e-3;
                        // Until the drag is wide enough, keep the previous region instead of wiping it.
                        self.project.timeline.region = if wide { Some((a, b)) } else { *prev };
                    }
                    DragKind::RegionStart => {
                        if let Some((_, b)) = self.project.timeline.region {
                            let a = self.snap_point(lay, t.max(0.0)).min(b - 1.0 / self.fps());
                            self.project.timeline.region = Some((a, b));
                        }
                    }
                    DragKind::RegionEnd => {
                        if let Some((a, _)) = self.project.timeline.region {
                            let b = self.snap_point(lay, t).max(a + 1.0 / self.fps());
                            self.project.timeline.region = Some((a, b));
                        }
                    }
                    DragKind::RegionMove { orig } => {
                        let mut d = dt.max(-orig.0);
                        if let Some(o) = self.snap_offset(lay, &[orig.0 + d, orig.1 + d], &HashSet::new()) {
                            d += o;
                        }
                        d = self.frame_snap(d);
                        self.project.timeline.region = Some((orig.0 + d, orig.1 + d));
                    }
                    DragKind::Move { ids, audio_lane } => {
                        let mut tl = drag.orig.clone();
                        let mut d = tl.clamp_move(ids, dt);
                        let times: Vec<f64> = tl.all_clips().filter(|(_, c)| ids.contains(&c.id)).flat_map(|(_, c)| [c.start + d, c.end() + d]).collect();
                        match self.snap_offset(lay, &times, ids) {
                            Some(o) => d += o,
                            None => d = self.frame_snap(d),
                        }
                        d = tl.clamp_move(ids, d);
                        let dtrack = if *audio_lane && !tl.audio.is_empty() { ((p.y - drag.origin_y) / AUDIO_H).round() as i32 } else { 0 };
                        tl.move_clips(ids, d, dtrack);
                        tl.overwrite_with(ids);
                        self.project.timeline = tl;
                        self.mark_drag_changed();
                    }
                    DragKind::TrimLeft { ids } | DragKind::TrimRight { ids } => {
                        let left = matches!(drag.kind, DragKind::TrimLeft { .. });
                        let mut tl = drag.orig.clone();
                        let edges: Vec<f64> = tl.all_clips().filter(|(_, c)| ids.contains(&c.id)).map(|(_, c)| if left { c.start } else { c.end() }).collect();
                        let mut d = dt;
                        let moved: Vec<f64> = edges.iter().map(|e| e + d).collect();
                        match self.snap_offset(lay, &moved, ids) {
                            Some(o) => d += o,
                            None => d = self.frame_snap(d),
                        }
                        if left {
                            tl.trim_left(ids, d);
                        } else {
                            let durs: Vec<f64> = self.media.iter().map(|m| m.duration).collect();
                            tl.trim_right(ids, d, &durs);
                        }
                        tl.overwrite_with(ids);
                        self.project.timeline = tl;
                        self.mark_drag_changed();
                        // Show the frame at the edge being trimmed.
                        let edge = self.project.timeline.all_clips().filter(|(_, c)| ids.contains(&c.id)).map(|(_, c)| if left { c.start } else { c.end() - 1.0 / self.fps() }).next();
                        if let Some(e) = edge
                            && !self.playing
                        {
                            self.cursor_preview(e);
                        }
                    }
                }
            }
        }

        // ---- drag end ----
        if resp.drag_stopped() || (self.drag.is_some() && !ctx.input(|i| i.pointer.any_down())) {
            if let Some(drag) = self.drag.take() {
                if drag.changed {
                    self.push_undo(drag.orig);
                    self.timeline_changed();
                }
            }
        }
    }

    fn mark_drag_changed(&mut self) {
        if let Some(d) = &mut self.drag {
            d.changed = true;
        }
    }

    /// Show the frame at `t` without moving the edit cursor.
    fn cursor_preview(&mut self, t: f64) {
        self.preview_at(t.max(0.0));
    }

    /// What grabbing the region bar at screen x would do (pin, body), if a region is there.
    fn region_hit(&self, lay: &Layout, x: f32) -> Option<DragKind> {
        let (a, b) = self.project.timeline.region?;
        let (xa, xb) = (lay.x(a), lay.x(b));
        let (da, db) = ((x - xa).abs(), (x - xb).abs());
        if da.min(db) <= PIN_PX {
            // Nearest pin wins (matters when the region is only a few pixels wide).
            return Some(if da < db || (da == db && x < xa) { DragKind::RegionStart } else { DragKind::RegionEnd });
        }
        (x > xa && x < xb).then_some(DragKind::RegionMove { orig: (a, b) })
    }

    fn snap_point(&self, lay: &Layout, t: f64) -> f64 {
        match self.snap_offset(lay, &[t], &HashSet::new()) {
            Some(o) => t + o,
            None => self.frame_snap(t),
        }
    }

    // ───────────── drawing ─────────────

    fn draw_timeline(&self, ui: &Ui, painter: &egui::Painter, lay: &Layout, full: Rect) {
        // backgrounds
        painter.rect_filled(full, 0.0, theme::TIMELINE_BG);
        painter.rect_filled(lay.region, 0.0, theme::REGION_BAR_BG);
        painter.rect_filled(lay.ruler, 0.0, theme::RULER_BG);
        for (i, (tr, r)) in lay.lanes.iter().enumerate() {
            let bg = if matches!(tr, TrackRef::Video) { theme::LANE_VIDEO } else if i % 2 == 0 { theme::LANE_A } else { theme::LANE_B };
            painter.rect_filled(*r, 0.0, bg);
            painter.line_segment([pos2(full.left(), r.bottom()), pos2(full.right(), r.bottom())], Stroke::new(1.0, theme::LINE));
        }

        // ruler ticks
        let min_label_px = 90.0;
        let steps = [1.0 / 60.0, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];
        let major = steps.iter().copied().find(|s| s * lay.pps >= min_label_px).unwrap_or(3600.0);
        let minor = major / 5.0;
        let t_start = lay.t(lay.lanes_x).max(0.0);
        let t_end = lay.t(lay.lanes_right);
        let mut k = (t_start / minor).floor() as i64;
        loop {
            let t = k as f64 * minor;
            if t > t_end {
                break;
            }
            let x = lay.x(t);
            let is_major = (k % 5) == 0;
            let h = if is_major { RULER_H * 0.55 } else { RULER_H * 0.25 };
            painter.line_segment([pos2(x, lay.ruler.bottom() - h), pos2(x, lay.ruler.bottom())], Stroke::new(1.0, theme::DIM));
            if is_major {
                let label = if major < 1.0 { fmt_time(t)[3..].to_string() } else { fmt_time(t)[..8].to_string() };
                painter.text(pos2(x + 3.0, lay.ruler.top() + 2.0), Align2::LEFT_TOP, label, FontId::monospace(10.0), theme::TEXT);
                painter.line_segment([pos2(x, lay.lanes[0].1.top()), pos2(x, full.bottom())], Stroke::new(1.0, theme::GRID));
            }
            k += 1;
        }

        // region
        painter.line_segment([lay.region.left_bottom(), lay.region.right_bottom()], Stroke::new(1.0, theme::LINE));
        if let Some((a, b)) = self.project.timeline.region {
            let (xa, xb) = (lay.x(a), lay.x(b));
            let bar = Rect::from_min_max(pos2(xa, lay.region.top() + 3.0), pos2(xb, lay.region.bottom() - 3.0));
            let shown = bar.intersect(lay.region);
            painter.rect_filled(shown, 2.0, theme::REGION);
            let label = fmt_time(b - a);
            if shown.width() > 110.0 {
                painter.text(shown.center(), Align2::CENTER_CENTER, label, FontId::monospace(11.0), Color32::WHITE);
            }
            let lanes_area = Rect::from_min_max(pos2(xa, lay.ruler.top()), pos2(xb, full.bottom())).intersect(Rect::from_min_max(pos2(lay.lanes_x, full.top()), full.max));
            painter.rect_filled(lanes_area, 0.0, theme::REGION_SHADE);
            for x in [xa, xb] {
                if x >= lay.lanes_x {
                    let tri = vec![pos2(x - 8.0, lay.region.top()), pos2(x + 8.0, lay.region.top()), pos2(x, lay.region.bottom())];
                    painter.add(egui::Shape::convex_polygon(tri, theme::REGION_PIN, Stroke::NONE));
                    painter.line_segment([pos2(x, lay.region.bottom()), pos2(x, full.bottom())], Stroke::new(1.0, theme::REGION_PIN));
                }
            }
        } else {
            painter.text(
                pos2(lay.lanes_x + 10.0, lay.region.center().y),
                Align2::LEFT_CENTER,
                "Drag here (or in empty track space) to set the render region  ·  I / O at cursor",
                FontId::proportional(11.0),
                theme::DIM,
            );
        }

        // clips
        let lanes_clip = Rect::from_min_max(pos2(lay.lanes_x, full.top()), full.max);
        for (tr, lane) in &lay.lanes {
            for c in self.project.timeline.clips(*tr) {
                let r = Rect::from_min_max(pos2(lay.x(c.start), lane.top() + 2.0), pos2(lay.x(c.end()), lane.bottom() - 2.0));
                if r.right() < lay.lanes_x || r.left() > lay.lanes_right {
                    continue;
                }
                let p = painter.with_clip_rect(r.intersect(lanes_clip));
                self.draw_clip(ui, &p, *tr, c, r, lay);
            }
        }

        // region pin lines stay visible over clips
        if let Some((a, b)) = self.project.timeline.region {
            for x in [lay.x(a), lay.x(b)] {
                if x >= lay.lanes_x {
                    painter.line_segment([pos2(x, lay.region.bottom()), pos2(x, full.bottom())], Stroke::new(1.0, theme::REGION_PIN));
                }
            }
        }

        // cursor
        let cx = lay.x(self.cursor);
        if cx >= lay.lanes_x {
            painter.line_segment([pos2(cx, lay.ruler.top()), pos2(cx, full.bottom())], Stroke::new(1.5, theme::CURSOR));
            let tri = vec![pos2(cx - 6.0, lay.ruler.top()), pos2(cx + 6.0, lay.ruler.top()), pos2(cx, lay.ruler.top() + 9.0)];
            painter.add(egui::Shape::convex_polygon(tri, theme::CURSOR, Stroke::NONE));
        }

        // header column background (drawn over clips scrolled left)
        let head = Rect::from_min_max(full.min, pos2(lay.lanes_x, full.bottom()));
        painter.rect_filled(head, 0.0, theme::HEADER_BG);
        painter.line_segment([pos2(lay.lanes_x, full.top()), pos2(lay.lanes_x, full.bottom())], Stroke::new(1.0, theme::LINE));
        painter.text(
            pos2(full.left() + 8.0, full.top() + (REGION_H + RULER_H) / 2.0),
            Align2::LEFT_CENTER,
            fmt_time(self.cursor),
            FontId::monospace(15.0),
            theme::ACCENT,
        );
    }

    fn draw_clip(&self, _ui: &Ui, p: &egui::Painter, tr: TrackRef, c: &Clip, r: Rect, lay: &Layout) {
        let selected = self.selection.contains(&c.id);
        let is_video = tr == TrackRef::Video;
        let (body, head) = match (is_video, selected) {
            (true, false) => (theme::VCLIP, theme::VCLIP_HEAD),
            (true, true) => (theme::VCLIP_SEL, theme::VCLIP_HEAD_SEL),
            (false, false) => (theme::ACLIP, theme::ACLIP_HEAD),
            (false, true) => (theme::ACLIP_SEL, theme::ACLIP_HEAD_SEL),
        };
        p.rect_filled(r, 3.0, body);
        let head_r = Rect::from_min_max(r.min, pos2(r.right(), r.top() + 15.0));
        p.rect_filled(head_r, 3.0, head);
        let content = Rect::from_min_max(pos2(r.left(), head_r.bottom()), r.max);

        if is_video {
            if let Some(thumbs) = self.thumbs.get(&c.media).filter(|t| !t.is_empty()) {
                let th = content.height() - 2.0;
                let sz = thumbs[0].1.size_vec2();
                let tw = (th * sz.x / sz.y).max(8.0);
                let mut x = r.left().max(lay.lanes_x - tw);
                while x < r.right().min(lay.lanes_right) {
                    let src = c.src_at(lay.t(x));
                    let i = thumbs.partition_point(|(t, _)| *t <= src).saturating_sub(1);
                    let tex = &thumbs[i].1;
                    let rr = Rect::from_min_size(pos2(x, content.top() + 1.0), vec2(tw, th));
                    p.image(tex.id(), rr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::from_white_alpha(if selected { 255 } else { 215 }));
                    x += tw;
                }
            }
        } else if let Some(peaks) = self.waveforms.get(&(c.media, c.stream)) {
            let mid = content.center().y;
            let half = content.height() / 2.0 - 2.0;
            let x0 = r.left().max(lay.lanes_x).floor();
            let x1 = r.right().min(lay.lanes_right).ceil();
            let col = if selected { theme::WAVE_SEL } else { theme::WAVE };
            let mut x = x0;
            while x < x1 {
                let s0 = c.src_at(lay.t(x)) * PEAKS_PER_SEC;
                let s1 = c.src_at(lay.t(x + 1.0)) * PEAKS_PER_SEC;
                let (i0, i1) = (s0.max(0.0) as usize, (s1.max(0.0) as usize).max(s0.max(0.0) as usize + 1));
                let peak = peaks.get(i0..i1.min(peaks.len())).map(|s| s.iter().fold(0f32, |m, v| m.max(*v))).unwrap_or(0.0);
                if peak > 0.002 {
                    let h = (peak.sqrt() * half).max(0.5);
                    p.line_segment([pos2(x + 0.5, mid - h), pos2(x + 0.5, mid + h)], Stroke::new(1.0, col));
                }
                x += 1.0;
            }
            p.line_segment([pos2(r.left(), mid), pos2(r.right(), mid)], Stroke::new(1.0, col.gamma_multiply(0.4)));
        }

        let name = self.media.get(c.media).map(|m| m.name()).unwrap_or_default();
        let label = if c.group == 0 { format!("{name}  (unlinked)") } else { name };
        p.text(pos2(r.left().max(lay.lanes_x) + 4.0, head_r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(11.0), Color32::WHITE);
        let stroke = if selected { Stroke::new(1.5, Color32::WHITE) } else { Stroke::new(1.0, Color32::from_black_alpha(160)) };
        p.rect_stroke(r, 3.0, stroke, StrokeKind::Inside);
    }

    fn track_headers(&mut self, ui: &mut Ui, lay: &Layout, full: Rect) {
        let mut changed_audio = false;
        let lanes = lay.lanes.clone();
        for (tr, lane) in lanes {
            let r = Rect::from_min_max(pos2(full.left(), lane.top()), pos2(lay.lanes_x, lane.bottom())).shrink2(vec2(6.0, 4.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
            match tr {
                TrackRef::Video => {
                    child.label(egui::RichText::new("🎞  Video").strong());
                    child.label(egui::RichText::new("gaps render black").small().color(theme::DIM));
                }
                TrackRef::Audio(i) => {
                    let t = &mut self.project.timeline.audio[i];
                    child.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(90.0).font(egui::TextStyle::Small));
                        let m = ui.add(egui::Button::new(egui::RichText::new("M").small()).selected(t.muted).min_size(vec2(20.0, 18.0))).on_hover_text("Mute");
                        if m.clicked() {
                            t.muted = !t.muted;
                            changed_audio = true;
                        }
                        let s = ui.add(egui::Button::new(egui::RichText::new("S").small()).selected(t.solo).min_size(vec2(20.0, 18.0))).on_hover_text("Solo");
                        if s.clicked() {
                            t.solo = !t.solo;
                            changed_audio = true;
                        }
                    });
                    let resp = child.add(egui::Slider::new(&mut t.volume_db, -40.0..=12.0).suffix(" dB").step_by(0.5).show_value(true));
                    if resp.double_clicked() {
                        t.volume_db = 0.0;
                    }
                    if resp.changed() || resp.double_clicked() {
                        changed_audio = true;
                    }
                }
            }
        }
        if changed_audio {
            self.audio_changed();
        }
    }
}
