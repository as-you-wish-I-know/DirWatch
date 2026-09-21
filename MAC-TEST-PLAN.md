# DirWatch — macOS Test Plan (build 2026-09-07.i27)

**Goal today:** (a) run the automated Mac gate, (b) sign off the two `.i24` keybindings that have
never been confirmed on any Mac (Cmd-W, Cmd-F), and (c) **close the top open gap — actually RUN
DirWatch on Apple Silicon (arm64)**, which has only ever been compiled, never run.

You have both an Intel and an Apple Silicon Mac available. The plan is written so the **Apple
Silicon** run is the priority; the Intel run is a quick re-confirm.

There is **no `.app` bundle** — you run the bare universal binary `dist/dirwatch` from Terminal.
That is expected for testing; a real `.app`/`.dmg` is a later release-prep item.

## Prerequisites (once per Mac)

- Xcode command-line tools: `xcode-select --install` (if not already present).
- Rust toolchain (`rustup`) with both Apple targets — `runtests.command` adds them itself, but you
  can pre-add: `rustup target add aarch64-apple-darwin x86_64-apple-darwin`.

## Setup (each Mac)

1. Create/empty a `test/` folder, extract `DirWatch_iced_20260907.i27.zip` into it so the files land
   at the top of `test/` (no wrapper folder).
2. From a terminal in `test/`:  `bash runtests.command`
   (or double-click `runtests.command` in Finder — it's `.command` for that reason).

## Part 1 — Automated gate (both Macs, but the arm64 build is the one that matters)

1. `bash runtests.command` — it ensures both Apple targets, runs fmt / clippy / build (both arches) /
   test / `--selftest`, `lipo`s a **universal** `dist/dirwatch`, size-checks it, confirms **no sidecar
   `.dylib`**, and writes `artifacts_2026-09-07.i27.zip` at the top of `test/`.
   - **Observe:** the run ends `RESULT: PASS`. Confirm the log line `lipo -info … : dist/dirwatch is
     architecture: x86_64 arm64` (proves the universal binary carries both).
   - **Upload** `artifacts_2026-09-07.i27.zip`. If it's missing, upload whatever landed at the top of
     `test/` and say so.

## Part 2 — `.i24` keybinding sign-off (do on EITHER Mac — never confirmed on any Mac yet)

These shipped in `.i24` (Cmd-W / Cmd-F on macOS) and have not been tested on hardware.

2. Launch: `./dist/dirwatch <some-dir-with-a-.log>`  (or `./dist/dirwatch .` inside a folder with a
   `.log`/`.txt` file). Click a file tile to open its **tail window**.
3. In the tail window press **Cmd-W** → the tail window closes. (Confirm **Ctrl-W** still closes one
   too — it should on all OSes.)
4. In the tail window press **Cmd-F** → the search bar focuses (cursor in the find box). Type a term
   present in the file → confirm it highlights/【jumps to】the match. (Confirm **Ctrl-F** still works.)

## Part 3 — Apple Silicon RUN (THE priority — arm64 has never been run)

Do all of this on the **Apple Silicon** Mac, running the universal `./dist/dirwatch`. Everything
below was PASSed on Intel (R88) but is UNVERIFIED on arm64.

5. **It launches and the GUI appears** on Apple Silicon at all. (The single most important data
   point — "runs on Apple Silicon" has never been confirmed.)
6. **Auto-open + live tail:** append lines to a watched `.log` (`echo hi >> watched.log` in a loop) →
   its tail window auto-opens and follows the new lines; scroll up pauses follow, bottom resumes.
7. **Custom patterns:** launch with `--patterns "*.log"` on a dir with both `.log` and `.txt` →
   only the `.log` is watched, the `.txt` is ignored.
8. **Large / fast-growing log:** point it at (or generate) a ~20 MB rapidly-appended `.log` → the
   tail stays smooth, no freeze (the virtualized-render path, validated on Intel only).
9. **Many files + cap:** drop many matching files into the dir at once → window-cap holds, the
   over-cap count **blinks**.
10. **Rotation / truncate:** truncate-and-rewrite a watched file in place → its tail resets cleanly
    (shows the `--- file rotated/truncated ---` marker), keeps following.
11. **Browse (native NSOpenPanel):** click **Browse…** → the native macOS folder picker opens; pick a
    folder, then **Restart** → it switches to watching that folder. (Note: Browse takes effect on
    Restart, not immediately — that's correct.)
12. **Long filenames (new in .i27):** put a file with a long name (e.g.
    `WinEventLogTool_debug_20260907_104223.log`) in a watched dir alongside short-named ones →
    that directory's tiles widen (double width, fewer columns), the long name shows in full, nothing
    bleeds into a neighbour; hover a clipped tile to see the full name in a tooltip. Short-name
    directories look normal.
13. **CLI output (macOS uses the terminal, not a window):** run `./dist/dirwatch --help`,
    `--version`, `--license` from the terminal → each prints to the terminal (on macOS there is no
    subsystem problem, so no window — that's correct and different from Windows).

## Part 4 — Intel re-confirm (quick, if time)

14. On the **Intel** Mac, run the same universal `./dist/dirwatch` and spot-check: it launches, a tail
    opens and follows, Cmd-W/Cmd-F work, and the long-filename tiles look right. (Intel was the
    prior sign-off machine; this just confirms the `.i27` universal binary is good on both arches.)

## What a pass unlocks

- Part 1 PASS on arm64 + Parts 2–3 PASS = **the Apple Silicon RUN gap is closed** and the `.i24`
  keybindings are signed off — the two things standing between here and a **macOS release**.
- Report per numbered item (pass / fail / didn't-run). Anything not reported is treated as unrun.
