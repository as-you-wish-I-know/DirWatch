# PORT-PLAN — Cross-platform (Windows + macOS + Linux)

**Status: SCOPING DOC. This authorizes NO build.** Per the collaboration prompt
(Rule 0), the next chat scopes with the user and builds only on the word "build." The toolkit
call (iced) and the scoping decisions in §7 are agreed, but no port code has been written.
Scoping is essentially COMPLETE as of 2026-07-26 — see §7. (A throwaway HTML look-mock was
produced 2026-07-26 to settle the theme question — not iced code.)

**Author's note for the next session:** produced in a scoping conversation on 2026-07-26
and updated same day with the user's scoping answers (§7). Read `PORT-PLAN-rust.md` (the
Windows/NWG port map), `DECISIONS.md`, and the handoff before acting on any of this. Treat
this as a companion to `PORT-PLAN-rust.md`, not a replacement.

---

## 1. Goal

Take the current Rust port of DirWatch — today a **Windows-only** app whose entire GUI is
built on `native-windows-gui` (NWG, a thin Win32 wrapper) — and make it run natively on
**Windows, macOS, and Linux from a single GUI codebase**. The behavior is fully specified
and signed off (see handoff "FEATURE-COMPLETE" status, R52); this is a re-expression of a
known-good design in a portable toolkit, NOT a redesign. **iced replaces NWG entirely** —
it becomes the only GUI on all three OSes; NWG is retired once iced reaches Windows parity.

## 2. The clean split: what survives vs. what is rebuilt

The reason this is a *port* and not a *rewrite* is that DirWatch's architecture already
isolated everything platform-specific into the GUI layer.

**Survives essentially untouched — `dirwatch-core`:**

- `schedule::SweepScheduler` (poll cadence + leading-edge notify debounce, mock-clock tested)
- the `SessionModel` (auto-open policy / `mark_activity` / `wants_open` / cap logic)
- glob matching (std-only), the `TailReader`, `WatchService::sweep` (sole authority)
- All of this is already platform-agnostic and unit-tested. It does not get touched beyond
  possibly a small trait to abstract "open/close a window" if the GUI wants one.

**Free on all three OSes — the `notify` crate:**

- Filesystem watching is already cross-platform: inotify on Linux, FSEvents/kqueue on macOS,
  ReadDirectoryChangesW on Windows. The `runtime.rs` wiring (background thread owns
  `WatchService` + `notify` watcher + sweep scheduler, sends `WatchEvent`s over an mpsc
  channel) is portable in shape; only minor per-OS wrinkles expected.

**Thrown away and rebuilt — everything under `cfg(windows)` GUI:**

- `gui.rs`, `tail_window.rs`, `tile_bitmap.rs`, `gui_selftest.rs`
- the console-attach / `FreeConsole` / `SetConsoleCtrlHandler` logic (Win32-only)
- ALL the Win32 subclassing, `WM_NCCALCSIZE`, `raw_borderless_edit`, `CreateWindowExW`,
  ComCtl32 manifest, `PrintWindow` screenshots — the entire R14–R52 body of work.

That last bullet is the cost of this port. Be honest about it with the user: the input-box
centering saga (R34–R51), the painted tiles, the scroll-repaint fixes — that Win32-specific
effort does not carry over. The *design decisions* it produced (layout, colors, behavior) do.

## 3. Toolkit decision: iced

**Chosen: `iced`** (retained-mode, Elm-style architecture). Agreed 2026-07-26.

### Why iced won

The deciding requirement is that **the per-file tail windows must be true independent OS
windows** (draggable to another monitor, separate in the taskbar/dock) — not panels docked
inside one app window. That requirement is load-bearing for DirWatch and it eliminated the
front-runner (egui).

Two axes decided it:

1. **First-class multi-window.** iced models each window as a real entity in its
   update/view loop, so "spawn a tail window per file, cap them, close one and free the
   slot" maps directly onto the framework instead of being bolted on.
2. **Architecture match.** iced's message loop (messages in → state update → re-view) is
   almost exactly the shape DirWatch already has: `SessionModel` emits `WatchEvent`s over an
   mpsc channel that a timer drains and repaints. Porting the core into iced is
   re-expressing a proven architecture, not adopting a new paradigm.

### Alternatives considered and why they lost (for the record, so this isn't re-litigated)

- **egui** (immediate-mode): trivial tile recoloring / autoscroll and *uniquely* good
  headless-screenshot verification (renders to an offscreen buffer — a real CI harness,
  impossible with NWG). BUT multi-window is its weakest, least-mature area (viewport API is
  newer, bolted onto a single-surface model). With independent OS windows as a hard
  requirement, egui's soft spot becomes load-bearing. **Rejected on the multi-window
  requirement.** (If tail views were ever acceptable as panels-in-one-window, egui would be
  the answer — noted in case the UX model ever changes.)
- **slint** (retained, `.slint` DSL): best-looking out of the box, declarative layout would
  have made the centering saga a non-issue, multi-window supported. BUT adds a DSL as a new
  moving part AND carries a dual-licensing question (royalty-free under conditions, plus
  commercial/GPL options) that must be cleared before shipping. iced is pure Rust with no
  license strings — matches the project's current posture. **Rejected to avoid the DSL +
  licensing overhead.**
- **gtk-rs** (native GTK): flawless multi-window, most mature. BUT native only on Linux,
  foreign on Windows/macOS, drags the GTK runtime into packaging (heaviest cross-platform
  ship), and keeps you closest to the "fighting a native toolkit's widget model" experience
  you're leaving NWG to escape. **Rejected — optimizes the one OS you already have.**
- **Tauri** (web frontend + Rust backend): easy multi-window and total layout freedom, but
  means adopting a web stack + an IPC seam the current pure-Rust design doesn't have.
  **Rejected — wrong shape for this project.**

### iced's accepted costs (eyes open)

- **Non-native look.** iced has its own theme; it will not look OS-native on any of the
  three. **RESOLVED (§7): the user accepts iced's default dark theme** after seeing the mock —
  consistency across OSes over native chrome. No effort spent matching the NWG look.
- **Opinionated architecture.** The GUI restructures around messages. (Low risk here — it's
  the shape the core already has.)
- **Harder headless verification than egui.** This is the real one — see §5. Mitigated by
  investing in the harness early (§7 decision).

## 4. Rebuild map (what the new GUI must reproduce, from the signed-off .NET `.9` parity)

Re-express, in iced, the behavior locked over R14–R52:

- Main window: Directory + Browse, Patterns pre-filled with defaults, Depth box, Start/Stop
  toggle, build ID in title, bottom status strip (`watching <dir> (<patterns>)  Open
  Windows: N of M`).
- Per-directory bordered boxes (root + each subdir, stacked/auto-grow).
- Tile grid: one tile per file, state color across the whole tile + border + filename;
  recolor on state change; click-to-open; active-closed BLINK cadence; cap-reached
  warning blink. **In iced this is trivial where NWG made it a saga** — recoloring is a
  state field, centered text is a property.
- Scrollable / resizable file area with dynamic tile columns.
- **Tail windows as independent OS windows** (the requirement): read-only, auto-scroll with
  follow, rotation/missing/reappear markers, edge-aware first placement + cascade, close
  frees the cap slot and recolors the tile.
- Settings dialog (P1 + P2): max windows, active timeout, auto-open, poll interval, legend.
- CLI (`--help`, flags, `--active-secs`, etc.) — this lives in the portable layer and
  should carry over with little change.

Layout constants and colors are already decided (see `PORT-PLAN-rust.md` / `DECISIONS.md`);
reuse the *values*, not the Win32 mechanism.

## 5. The sleeper cost: per-platform verification harness

**The biggest risk is verification, not the GUI code itself.** The entire current quality
loop is Windows-shaped: `runtests.cmd`, `--selftest-gui` PNGs via `PrintWindow`, hardware
sign-off on the user's machine. The handoff repeatedly names *off-hardware guessing* as the
expensive failure mode (the .r21–.r24 centering guesses, the .r29 dead-code override).

**DECISION (§7): build the automated harness EARLY and first-class**, so the assistant can
self-verify on all three OSes instead of routing every round through the user's eyeballs. iced
can render to an offscreen target, so an offscreen-render + scripted-state screenshot
harness is feasible. This is the direct antidote to off-hardware guessing and is what lets
"do as much as possible without the user" actually hold. Scope it as a deliverable of the port,
not an afterthought; stand it up alongside the skeleton so the tile/tail-window rebuild is
verified as it lands.

## 6. Suggested sequencing (for when a build IS authorized)

**Build order: Windows first, then Mac/Linux.** Rationale (the user's call, §7): Windows is the
dev + eyeball machine and the signed-off NWG build is the parity reference/fallback, so it's
both the easiest to build against and the lowest-risk — the working NWG build is never lost.
Bring iced to Windows parity, retire NWG, then fan out to Mac + Linux where the same iced
code should largely just run.

0. **Throwaway look mock — DONE (2026-07-26).** An HTML mock of the main window in iced's
   default dark theme (delivered for eyeball). NOT iced code. Result: the user accepted the
   default theme (§7). This step is closed.
1. Stand up an iced skeleton main window that renders the tile grid off `SessionModel`, wire
   the existing mpsc/`WatchEvent` drain into iced's update loop. Prove the core slots in.
2. Build the per-platform verification harness (offscreen render + state scripting) EARLY.
3. Rebuild inputs / buttons / Depth / status strip to `.9` parity (fast in iced).
4. Rebuild tail windows as independent OS windows — the riskiest multi-window piece; verify
   on all three OSes.
5. Settings dialog.
6. Windows parity sign-off → retire NWG.
7. Mac + Linux bring-up + the per-OS "does it look right" sign-off loop.
8. **Release prep (all deferred hygiene lands here — §7):** remove `--selftest`/`--selftest-gui`
   (folds in naturally — the iced harness already replaced `gui_selftest.rs`); run the
   adversarial code review on the final iced tree (core + new GUI); per-OS packaging
   (see §7 targets); final hardware sign-off + fresh handoff package.

Effort framing given to the user: not a weekend; on the order of the original GUI work but
faster the second time because behavior is fully specified. It is ONE clean rewrite covering
all three OSes and replacing NWG, not three separate ports.

## 7. Scoping decisions (the user, 2026-07-26) — COMPLETE

- **Cutover / build order — Windows first.** iced replaces NWG. Build the iced GUI on
  Windows first (dev machine; NWG signed-off build is the parity reference and stays
  available as fallback until iced matches it), then bring up Mac + Linux. Not a big-bang.
- **Look/theme — accept iced's default dark theme.** the user reviewed an HTML mock of the main
  window in iced's stock dark palette (2026-07-26) and chose "default theme is good enough."
  No effort spent matching the current NWG Windows appearance; the app looks consistent
  across all three OSes in iced's default look. (Reopen only if the user changes his mind after
  seeing real iced output.)
- **Verification — automated harness, early & first-class.** the user: "do as much of the work
  as possible without me." Interpreted as: invest in the offscreen-render self-verification
  harness up front so rounds don't route through his eyeballs. (See §5.)
- **Packaging targets (v1) — all four:**
  - macOS Apple Silicon (arm64)
  - macOS Intel (x86_64) — likely delivered as a **universal binary** covering both Mac arches
  - Linux AppImage / portable binary
  - Linux native packages (deb / rpm / flatpak)
- **macOS signing — UNSIGNED for v1.** Ship unsigned Mac binaries; users right-click-open
  past the Gatekeeper warning the first time. No Apple Developer ID account needed. Revisit
  signing/notarization later if the tool gets wider distribution.
- **Production cleanup (remove `--selftest`/`--selftest-gui`) — at RELEASE, not before.**
  Don't do a separate NWG cleanup build. The port replaces `gui_selftest.rs`/the Windows
  harness with the iced harness anyway, so the flag removal folds into release prep (§6.8).
- **Adversarial code review — DEFERRED to near-release.** Run it once on the final shipping
  iced tree (core + new GUI) when nearly ready to release, in the user's separate chat with
  `code-review-prompt-v3.md`. No review effort spent on the NWG GUI that's being retired.

## 8. Still-open questions

Scoping is complete for the decisions known at 2026-07-26. Nothing blocks a build-scoping
turn. Items that will surface naturally during the work (not blockers now):

- **`SessionModel` "open/close a window" trait** — decide the exact seam when wiring step 1
  (may need a small abstraction so the core can request a window without knowing the toolkit).
- **iced version pin** — pick the iced release + confirm the multi-window (`daemon`/multi-
  window) API shape at build time; the framework moves, so verify against current docs then.
- **Linux packaging mechanics** — which tool for deb/rpm/flatpak + AppImage (cargo-dist,
  cargo-bundle, etc.); a packaging-time detail, not a scoping decision.

---

*This doc reflects a scoping discussion on 2026-07-26, updated the same day with the user's
scoping answers (§7), including the theme decision after he reviewed the look-mock. Scoping
is complete. It records the iced decision and the reasoning behind it. No port build is
authorized by this document — per Rule 0, the build starts only on the user's word.*
