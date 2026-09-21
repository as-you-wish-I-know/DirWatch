# DirWatch — Input-box text vertical centering (SOLVED & SIGNED OFF PASS at .r35, R51)

**Status: CLOSED. SIGNED OFF PASS on the user's Windows (R51, .r35, 2026-07-16) — "it looks great."
.r34 (R49) landed the real fix; .r35 (R50) the 1px optical polish. ~16 rounds; done.**

**.r34 hardware result:** mystery border ABSENT (create-borderless fix works — style clean,
edge-dark fractions 0.00 before AND after a re-strip), labels ALIGNED (the user approved; harness top
delta +0), glyph ink 5/5 ink-centered, focus/typing/placeholder/tiles all good. Sole residual: text
read slightly TOP-HEAVY.

**Why (confirmed):** ink-centering counts the descender zone (p/g/_ — mostly empty air), so the
visible cap-to-baseline block sits ~5 above / ~8 below and reads top-heavy. Standard fix = a small
downward OPTICAL nudge.

**.r35 fix (R50):** `INPUT_EDIT_Y` 3→4 and `INPUT_LABEL_NUDGE` 3→4 IN LOCKSTEP (labels stay on the
glyph baseline, delta 0). Cap-block now ~6/7 — balanced. **The harness ink verdict now reads "rides
LOW by ~2px" BY DESIGN** (optical vs ink center differ by descender depth); it is annotated in code
and never fails the run. DO NOT re-center to ink 0/0 — that brings back the top-heavy look.

## The real root cause (R48/R49): the EDIT caches its border at CREATION
Style readback on hardware showed `WS_BORDER=false, exstyle=0, theme_open=false` — yet a **#646464
(COLOR_WINDOWFRAME)** ring persisted through a second strip. The Win32 EDIT class snapshots its
border state at creation, and NWG force-creates every TextInput with WS_BORDER; nothing after
creation reaches that cache. Fix (`raw_borderless_edit`): create the three edits with
`CreateWindowExW` — never WS_BORDER, ex=0, unthemed, WM_SETFONT — child of their painted frames, HWND
wrapped into `nwg::TextInput` via the public `ControlHandle::Hwnd` (all call sites unchanged; NWG's
hidden hooks never installed). Centering from measured ink: unthemed edit paints ink at +3 from its
top (themed was +5, R44).

## The .r31 composition defects (R46) — for the record
- Frame and edit were overlapping SIBLINGS; neither NWG ImageFrame nor TextInput has
  WS_CLIPSIBLINGS, so they overdrew each other in invalidation order: untouched run → frame's white
  bitmap painted last → TEXT INVISIBLE (the harness's "no ink" line); interaction → edit repainted
  on top → text visible again. The harness had the evidence as a WARN on a green run — now it FAILS.
- The themed EDIT kept rendering its border after WS_BORDER was stripped post-creation → our painted
  border PLUS an inner edit border ("box in a box"). Untheming removes the renderer, not just the
  style bit.
- Residual ±1px risk, stated: the R44 ink constants were measured on a THEMED edit; the unthemed one
  may render a hair differently — the harness reports exact ink rows + verdicts, so any residue is a
  single-constant tweak.

The R44 analysis below (why NO in-place fix of the themed EDIT was ever possible) stands unchanged.

## The R44 measurements (the user's hardware, 100% scaling) — why every prior attempt failed
- **`fired=0`** — the .r29 override handler NEVER RAN: it was bound (in `build_ui_for`) after
  `build()`'s `layout_chrome` had already triggered the boxes' only `WM_NCCALCSIZE`, and nothing
  re-triggered one. Lesson: after binding a subclass that must see NCCALCSIZE, force one with
  `SetWindowPos(... SWP_FRAMECHANGED)`.
- **`nc_top=0 nc_bot=0, client_h=24=window_h`** — the boxes have ZERO non-client area. **The themed
  EDIT paints its border INSIDE the client area, together with the text.** This is the decisive
  fact: any `WM_NCCALCSIZE` inset (NWG's `-4` hook, our override, anything) shifts border and text
  AS ONE — the text can never move relative to the visible box. Box height, EM_SETRECT, multiline,
  and NC insets (R34–R43) were all structurally incapable of working on this machine.
- **Ink scan: glyph rows 5..16 of 24** — ≈4px interior gap above the text, ≈6 below: the text truly
  renders ~2px high inside a geometrically symmetric box. (The .r30 "gap below=8" print overcounted
  by 1 — `h - ink_bot` instead of `h-1 - ink_bot`.)

## The .r31 fix (R45): painted frame + borderless snug EDIT
Each input is now TWO pieces:
- A bitmap-painted 24px **frame** (`nwg::ImageFrame` + `tile_bitmap::frame_bmp` — white fill, 1px
  explicit-color border: gray blurred, accent blue focused; repainted on resize + focus change). The
  same proven machinery as the tiles and the count field. The box the user sees is OURS.
- A **borderless, snug (18px) native EDIT** on top — caret, selection, cue banner, all native.
  WS_BORDER stripped post-build; a raw subclass per edit BLOCKS `WM_NCCALCSIZE` (returns the rect
  untouched — at 18px NWG's `-4` hook math goes NEGATIVE and would shift/clip the text up) and
  repaints the frame border on WM_SETFOCUS/WM_KILLFOCUS. An explicit SWP_FRAMECHANGED after binding
  guarantees the blocker actually runs (the `fired=0` lesson).

Centering constants, derived from the R44 ink scan (a top-pinned EDIT renders ink at rows 5..16 from
its own top; a 24px frame's interior centers 12 ink rows at 6..17):
`INPUT_FRAME_H=24, INPUT_EDIT_H=18, INPUT_EDIT_Y=1, INPUT_EDIT_PAD_X=3` (in `gui.rs`). Labels moved
to the frame-top y: NWG's label hook (−1 bias) centers a 16px line in a 22px label at label_y+6..+17
— the same rows as the box ink. **All constants are ±1px tunable**: the harness measures the real
result, so any residue is a one-constant tweak with numbers in hand.

## What the .r31 harness reports (run `runtests`; it runs `--selftest-gui` and collects everything)
- `nccalc blocker: fired=N` — expect ≥ 3.
- Per input: frame/edit sizes + `edit_offset=(x,y)` — expect (3,1).
- `Directory BOX ink: gap above / below` + CENTERED / rides HIGH / rides LOW verdict.
- `label vs box ink rows` + ALIGNED / off-by-N verdict — the label-alignment question, answered in
  pixels.
- `gui_r45_dirrow_4x.png` — 4× crop of the whole Directory row (label through box) for the eyeball.

## Scope boundaries / knowns
- The SETTINGS dialog's inputs are NOT converted (different window; not flagged by the user). Same
  treatment applies if wanted later.
- Focus border = standard accent blue (0,120,215); tunable.
- the user is at **100% display scaling** — constants assume 96 DPI. At other DPIs the ink math would
  need re-derivation (measure first, as ever).


---

# Historical record (superseded analyses kept for the audit trail)

## The NWG library hooks (found R43, refined by R44)
`nwg::TextInput::build()` in **native-windows-gui 1.0.13** (`src/controls/text_input.rs`,
`hook_non_client_size`) unconditionally installs a hidden `WM_NCCALCSIZE` subclass on every edit,
with a hardcoded `center = ((window_height - client_height) / 2) - 4`; `nwg::Label` has the same
hook with `- 1`. R43 attributed the whole saga to the `-4` bias. **R44 refined this:** at the user's
geometry the hook computes 0 (no inset at all), and more fundamentally the border is client-painted,
so even a "corrected" hook could not have centered the text. The hooks still matter in .r31 — at the
snug 18px edit height NWG's math goes NEGATIVE, which is why the blocker subclass must keep its hook
off our edits — but they were never the full story.

## The .r29 attempt (R43 — never executed on hardware)
Bound a corrected WM_NCCALCSIZE handler per input in `build_ui_for`. R44 proved `fired=0`: the bind
happened after `layout_chrome`'s only WM_NCCALCSIZE, so the handler was dead code. (Even had it
fired, the client-painted border means it could not have moved the text relative to the box.)

## KNOWN to fail (do NOT re-test — all explained by the client-painted border)
1. Box HEIGHT (22/24/26/~29 — R34/R35/R37/R41).
2. EM_SETRECT — single- or multi-line (R36/R38/R39; proven pixel-identical).
3. ES_MULTILINE conversion (R38).
4. Plain single-line boxes at height 24 (R41).
5. `EM_SETMARGINS` — horizontal-only.
6. Visibly shorter THEMED boxes — the user ruled out (R36). (The .r31 edit is short but invisible — the
   painted frame keeps the visible box at 24px.)
7. ANY WM_NCCALCSIZE inset on the themed bordered EDIT — border and text move together (R44).

## Full attempt log
- **R34 (.r20)** snug box height via GetTextMetricsW — no fix.
- **R35 (.r21)** snug height on all 3 + center on row — no fix.
- **R36 (.r22)** EM_SETRECT on single-line edit — no visible change.
- **R37 (.r23)** taller boxes so EM_SETRECT has room — no fix.
- **R38 (.r24)** MULTILINE + EM_SETRECT — no fix on hardware.
- **R39 (.r25)** harness #1: 6 EM_SETRECT variants — ALL 6 IDENTICAL.
- **R40 (.r26)** harness #2: box-offset rows — misread as "plain single-line centers fine."
- **R41 (.r27)** plain single-line height 24 — STILL HIGH per the user.
- **R42 (.r28)** handoff package; catalogued the problem.
- **R43 (.r29)** found the NWG hooks; built an override that (per R44) never ran.
- **R44 (.r30)** measure-first diagnostic: fired=0, zero NC, ink 5..16/24 — the decisive round.
- **R45 (.r31)** redesign: painted frame + borderless snug EDIT. GEOMETRY validated on hardware;
  COMPOSITION FAILED (invisible text + double border) — sign-off FAIL.
- **R46 (.r32)** composition fix: edits parented INSIDE frames + WS_CLIPCHILDREN + untheme.
  Text visible again, but the edit STILL draws a border ("double box") — sign-off FAIL (R47).
- **R47 (no build)** the user stopped; chose Option B (diagnose).
- **R48 (.r33)** forensics: border NOT style/theme — EDIT caches border at CREATION; #646464
  WINDOWFRAME ring; clean glyph offsets measured (+3 unthemed; label +3).
- **R49 (.r34)** the aimed fix: edits created RAW (never WS_BORDER); INPUT_EDIT_Y=3,
  INPUT_LABEL_NUDGE=3. Border gone + ink-centered on hardware; labels approved; text read top-heavy.
- **R50 (.r35)** +1px optical nudge: INPUT_EDIT_Y=4, INPUT_LABEL_NUDGE=4 (lockstep).
- **R51 (.r35)** SIGNED OFF PASS on the user's Windows — saga CLOSED.

## Where the code is
`crates/dirwatch/src/gui.rs`: constants (`INPUT_FRAME_H/INPUT_EDIT_H/INPUT_EDIT_Y/INPUT_EDIT_PAD_X`,
`INPUT_FILL/INPUT_BORDER/INPUT_BORDER_FOCUS`), `input_frames`/`input_frame_bmps`/`input_focused`,
`place_input`/`paint_input_frame`/`set_input_focus_visual`, `strip_border`/`force_frame_recalc`/
`untheme`/`add_clip_children`/`bind_edit_fullclient`, layout in `layout_chrome`
(`row1_center=25`/`row2_center=60`); edits are CHILDREN of `input_frames` (R46).
`crates/dirwatch/src/tile_bitmap.rs`: `frame_bmp`. `crates/dirwatch/src/gui_selftest.rs`:
`dump_center_geometry`, `ink_rows`, `crop_magnify_png`. Library reference: native-windows-gui 1.0.13
`src/controls/text_input.rs` (the `-4` hook) and `src/controls/label.rs` (the `-1` hook).
