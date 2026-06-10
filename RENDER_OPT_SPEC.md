# Render optimization spec — B.1 / B.2 / B.3

Goal: make the TUI feel closer to native by cutting interaction latency and
per-frame CPU, WITHOUT regressing the data-update throttle that fixed the
original lag. Spec-driven + TDD: every item below has a behavior contract, an
acceptance bar, and the tests that prove it. Tests are written first (red),
then implementation (green).

Shared principle: data events stay throttled (one redraw per frame); only the
things a human waits on (keypress) get immediate feedback.

---

## B.1 — RenderScheduler: 60fps frame + key-immediate redraw

**Problem.** Fixed 33ms ticker means every input waits up to 33ms before its
effect is drawn, and the frame cap is 30fps.

**Behavior.**
- Frame interval is 16ms (~60fps), not 33ms.
- A redraw happens at a frame boundary only if state is `dirty`.
- A *key/interaction* event forces an immediate redraw (does not wait for the
  next frame boundary), so navigation/scroll feel instant.
- A *data* event (engine AppEvent) does NOT force immediate redraw — it only
  marks dirty and is coalesced into the next frame (preserves the lag fix).
- If nothing is dirty, no draw happens (no busy-spin).

**Unit under test:** `RenderScheduler` — pure decision logic, no terminal.
- `mark_dirty()` sets dirty.
- `on_key()` sets dirty AND arms an immediate-draw request.
- `should_draw_now()` → true if an immediate request is armed (consumes it).
- `should_draw_on_tick()` → true iff dirty (consumes dirty).
- `FRAME` constant = 16ms.

**Acceptance / tests (red first):**
1. fresh scheduler: `should_draw_on_tick()==false`, `should_draw_now()==false`.
2. after `mark_dirty()`: `should_draw_now()==false` (data never forces now),
   `should_draw_on_tick()==true`, and a second tick is `false` (consumed).
3. after `on_key()`: `should_draw_now()==true` (immediate), consumed on 2nd call.
4. `FRAME <= Duration::from_millis(16)`.

---

## B.2 — PaneCache: only rebuild the focused/dirty pane

**Problem.** Every frame rebuilds + renders all three panes (Steps, Agents,
Detail) into the frame buffer even when only one changed.

**Behavior.**
- Each pane caches the `Buffer` it last rendered, keyed by its `Rect` (area).
- On draw, a pane re-runs its render closure ONLY IF its area changed OR the
  pane is marked dirty; otherwise it blits the cached Buffer.
- Marking: steps/agents panes dirty on data events that touch them and on focus
  change; detail dirty on selected-agent change / scroll / its agent's updates.

**Unit under test:** `PaneCache` — holds `Option<(Rect, Buffer)>`.
- `render_into(frame_area, force, render_fn)`:
  - calls `render_fn(area, &mut buf)` and caches, when cache empty / area
    changed / `force==true`;
  - else reuses cached buffer;
  - returns whether it was a rebuild (for test assertions) and merges the
    buffer into the target.

**Acceptance / tests (red first):**
1. first call → rebuild=true; render_fn invoked once.
2. second call, same area, force=false → rebuild=false; render_fn NOT invoked
   again (cache hit).
3. force=true → rebuild=true (invoked again).
4. area changed → rebuild=true (invoked again).
Use a counter closure to assert invocation counts; a `TestBackend` Buffer target.

---

## B.3 — WrapCache: width-aware CJK wrap of Detail, cached

**Problem.** Detail uses `Paragraph::wrap` which re-wraps every frame; with CJK
(width-2) text the default wrap is also width-naive.

**Behavior.**
- Wrap detail lines to the pane inner width using display width
  (unicode-width: CJK = 2 cols), breaking at width budget.
- Cache the wrapped `Vec<String>` keyed by `(content_version, width)`. Rebuild
  only when the agent's detail grew (version bumped) or width changed.
- Feed the Detail Paragraph the PRE-WRAPPED lines with wrap disabled, so ratatui
  does not re-wrap.

**Unit under test:** `wrap_width(line, width) -> Vec<String>` + `WrapCache`.
- `wrap_width`:
  1. ASCII longer than width splits into chunks each ≤ width display cols.
  2. CJK string: each output line's display width ≤ width (width-2 chars
     counted as 2), so e.g. width=4 with "你好世界" → 2 chars per line.
  3. line shorter than width returned as-is (one element).
  4. empty string → `[""]` (one empty line, preserved).
- `WrapCache.get(version, width, build_fn)`: build_fn runs once for same
  (version,width); re-runs when version changes; re-runs when width changes.

**Acceptance:** all four `wrap_width` cases + three cache cases pass.

---

## Verification (B.* together)
- `cargo test` green (all units).
- `cargo build` clean.
- Re-bench: report frame interval change (33→16ms) and per-frame cost with pane
  caching vs without, on the same deterministic load. Numbers in the final
  report.
