//! Render-optimization units (spec: RENDER_OPT_SPEC.md).
//!
//! B.1 RenderScheduler  — 60fps frame + key-immediate redraw.
//! B.2 PaneCache        — only rebuild the focused/dirty pane.
//! B.3 wrap_width/WrapCache — width-aware CJK wrap of Detail, cached.
//!
//! These are deliberately small, terminal-light units so they can be unit
//! tested directly (TDD). The TUI loop composes them.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

// ── B.1 RenderScheduler ───────────────────────────────────────────────────

/// Decides WHEN to draw. Data events coalesce to the next ~16ms frame; key
/// events force an immediate draw so input feels instant.
pub struct RenderScheduler {
    dirty: bool,
    immediate: bool,
}

impl RenderScheduler {
    /// ~60fps. Frame boundary cadence for coalesced (data) redraws.
    pub const FRAME: Duration = Duration::from_millis(16);

    pub fn new() -> Self {
        Self {
            dirty: false,
            immediate: false,
        }
    }

    /// A data event changed state: redraw at the next frame, not now.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// A key/interaction happened: redraw immediately (and it is also dirty).
    pub fn on_key(&mut self) {
        self.dirty = true;
        self.immediate = true;
    }

    /// True if an immediate (key-driven) redraw is pending. Consumes the
    /// immediate flag AND the dirty flag (the draw will satisfy both).
    pub fn should_draw_now(&mut self) -> bool {
        if self.immediate {
            self.immediate = false;
            self.dirty = false;
            true
        } else {
            false
        }
    }

    /// Called at a frame boundary: draw iff dirty. Consumes dirty.
    pub fn should_draw_on_tick(&mut self) -> bool {
        if self.dirty {
            self.dirty = false;
            self.immediate = false;
            true
        } else {
            false
        }
    }
}

impl Default for RenderScheduler {
    fn default() -> Self {
        Self::new()
    }
}

// ── B.2 PaneCache ─────────────────────────────────────────────────────────

/// Caches the Buffer a pane last rendered, keyed by area. Re-renders only when
/// the area changes or the pane is forced dirty; otherwise reuses the cache.
pub struct PaneCache {
    cached: Option<(Rect, Buffer)>,
}

impl PaneCache {
    pub fn new() -> Self {
        Self { cached: None }
    }

    /// Render this pane into `target` at `area`. Re-invokes `render_fn` only on
    /// cache miss (empty / area changed / `force`). Returns true if it rebuilt.
    pub fn render_into<F>(
        &mut self,
        target: &mut Buffer,
        area: Rect,
        force: bool,
        render_fn: F,
    ) -> bool
    where
        F: FnOnce(Rect, &mut Buffer),
    {
        let hit = match &self.cached {
            Some((a, _)) if *a == area && !force => true,
            _ => false,
        };
        if !hit {
            let mut buf = Buffer::empty(area);
            render_fn(area, &mut buf);
            self.cached = Some((area, buf));
        }
        if let Some((_, buf)) = &self.cached {
            target.merge(buf);
        }
        !hit
    }
}

impl Default for PaneCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── B.3 wrap_width + WrapCache ────────────────────────────────────────────

/// Wrap a single logical line to `width` DISPLAY columns (CJK counts as 2).
/// Never splits a char; an empty input yields one empty line.
pub fn wrap_width(line: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in line.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if cur_w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += cw;
    }
    out.push(cur); // pushes "" for empty input → preserves blank line
    out
}

/// Caches wrapped detail lines keyed by (content_version, width). Rebuilds only
/// when the agent's detail grew (version bump) or the pane width changed.
pub struct WrapCache {
    key: Option<(u64, u16)>,
    lines: Vec<String>,
}

impl WrapCache {
    pub fn new() -> Self {
        Self {
            key: None,
            lines: Vec::new(),
        }
    }

    /// Return cached wrapped lines, rebuilding via `build` only on key change.
    pub fn get<F>(&mut self, version: u64, width: u16, build: F) -> &[String]
    where
        F: FnOnce() -> Vec<String>,
    {
        if self.key != Some((version, width)) {
            self.lines = build();
            self.key = Some((version, width));
        }
        &self.lines
    }
}

impl Default for WrapCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests (TDD: written against the spec) ─────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // B.1
    #[test]
    fn scheduler_fresh_draws_nothing() {
        let mut s = RenderScheduler::new();
        assert!(!s.should_draw_on_tick());
        assert!(!s.should_draw_now());
    }

    #[test]
    fn scheduler_data_event_coalesces_to_tick_not_now() {
        let mut s = RenderScheduler::new();
        s.mark_dirty();
        assert!(!s.should_draw_now(), "data must not force immediate draw");
        assert!(s.should_draw_on_tick(), "dirty draws at frame boundary");
        assert!(!s.should_draw_on_tick(), "dirty consumed");
    }

    #[test]
    fn scheduler_key_event_draws_immediately() {
        let mut s = RenderScheduler::new();
        s.on_key();
        assert!(s.should_draw_now(), "key forces immediate draw");
        assert!(!s.should_draw_now(), "immediate consumed");
        assert!(!s.should_draw_on_tick(), "and dirty was consumed too");
    }

    #[test]
    fn scheduler_frame_is_60fps() {
        assert!(RenderScheduler::FRAME <= Duration::from_millis(16));
    }

    // B.2
    #[test]
    fn panecache_rebuilds_then_hits() {
        let calls = Cell::new(0);
        let area = Rect::new(0, 0, 10, 3);
        let mut target = Buffer::empty(area);
        let mut pc = PaneCache::new();
        let f = |a: Rect, b: &mut Buffer| {
            calls.set(calls.get() + 1);
            b.set_string(a.x, a.y, "hi", ratatui::style::Style::default());
        };
        assert!(pc.render_into(&mut target, area, false, f), "first = rebuild");
        assert_eq!(calls.get(), 1);
        // same area, not forced → cache hit, render_fn not called again
        let f2 = |_a: Rect, _b: &mut Buffer| calls.set(calls.get() + 1);
        assert!(!pc.render_into(&mut target, area, false, f2), "second = hit");
        assert_eq!(calls.get(), 1, "render_fn must NOT run on cache hit");
    }

    #[test]
    fn panecache_force_rebuilds() {
        let calls = Cell::new(0);
        let area = Rect::new(0, 0, 8, 2);
        let mut target = Buffer::empty(area);
        let mut pc = PaneCache::new();
        pc.render_into(&mut target, area, false, |_, _| calls.set(calls.get() + 1));
        assert!(pc.render_into(&mut target, area, true, |_, _| calls.set(calls.get() + 1)));
        assert_eq!(calls.get(), 2, "force must rebuild");
    }

    #[test]
    fn panecache_area_change_rebuilds() {
        let calls = Cell::new(0);
        let mut target = Buffer::empty(Rect::new(0, 0, 20, 5));
        let mut pc = PaneCache::new();
        pc.render_into(&mut target, Rect::new(0, 0, 8, 2), false, |_, _| calls.set(calls.get() + 1));
        let rebuilt = pc.render_into(&mut target, Rect::new(0, 0, 10, 2), false, |_, _| calls.set(calls.get() + 1));
        assert!(rebuilt, "area change must rebuild");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn panecache_composites_at_pane_offset() {
        // The pane must blit into a multi-pane target at its ABSOLUTE offset — the
        // exact concern when draw() composes panes into one frame buffer. Proves
        // ratatui's offset-aware Buffer::merge is used correctly.
        let mut target = Buffer::empty(Rect::new(0, 0, 40, 4));
        let mut pc = PaneCache::new();
        let pane = Rect::new(20, 0, 20, 4);
        pc.render_into(&mut target, pane, false, |a, b| {
            b.set_string(a.x, a.y, "R", ratatui::style::Style::default());
        });
        assert_eq!(target[(20, 0)].symbol(), "R", "drawn at pane offset");
        assert_eq!(target[(0, 0)].symbol(), " ", "other pane untouched");
        // Cache hit re-merges the cached buffer (content still present).
        let rebuilt = pc.render_into(&mut target, pane, false, |_, _| {});
        assert!(!rebuilt, "second call is a cache hit");
        assert_eq!(target[(20, 0)].symbol(), "R", "cached buffer re-merged");
    }

    // B.3
    #[test]
    fn wrap_ascii_splits_by_width() {
        let w = wrap_width("abcdefgh", 3);
        assert_eq!(w, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_cjk_counts_double_width() {
        // width 4 cols, each CJK char = 2 cols → 2 chars per line.
        let w = wrap_width("你好世界", 4);
        assert_eq!(w, vec!["你好", "世界"]);
        for line in &w {
            let cols: usize = line.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(1)).sum();
            assert!(cols <= 4);
        }
    }

    #[test]
    fn wrap_short_line_unchanged() {
        assert_eq!(wrap_width("hi", 80), vec!["hi"]);
    }

    #[test]
    fn wrap_empty_preserved() {
        assert_eq!(wrap_width("", 10), vec![""]);
    }

    #[test]
    fn wrapcache_builds_once_per_key() {
        let calls = Cell::new(0);
        let mut c = WrapCache::new();
        let build = || {
            calls.set(calls.get() + 1);
            vec!["x".to_string()]
        };
        c.get(1, 80, build);
        let build2 = || {
            calls.set(calls.get() + 1);
            vec!["x".to_string()]
        };
        c.get(1, 80, build2);
        assert_eq!(calls.get(), 1, "same (version,width) builds once");
    }

    #[test]
    fn wrapcache_rebuilds_on_version_and_width_change() {
        let calls = Cell::new(0);
        let mut c = WrapCache::new();
        let mk = || {
            // closure that bumps + returns
            calls.set(calls.get() + 1);
            vec!["x".to_string()]
        };
        c.get(1, 80, mk);
        c.get(2, 80, || { calls.set(calls.get() + 1); vec!["x".to_string()] }); // version change
        c.get(2, 40, || { calls.set(calls.get() + 1); vec!["x".to_string()] }); // width change
        assert_eq!(calls.get(), 3, "version or width change rebuilds");
    }
}
