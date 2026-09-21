# DirWatch — Rust port plan

Written 2026-07-14 for the fresh chat that will execute the port (DECISIONS entry 45). The
current .NET/Avalonia build `2026-07-14.9` (signed off PASS, entry 44) is the reference spec:
the port must reach behavioral parity with it, then earn its own hardware sign-off. This plan
is a starting point, not a contract — the port chat re-scopes with the user before building.

## Why the port exists (lead with the constraint, don't repeat the mistake)

The .NET build ships a ~90 MB self-contained single-file exe that self-extracts its bundled
native libs (Skia/HarfBuzz/ANGLE) to a temp dir on first launch. Both the size and the
extraction are artifacts of the Avalonia→Skia rendering stack. The Avalonia choice was made
for the ASSISTANT's headless-screenshot convenience and its deployment cost was never
surfaced to the user — that is the mistake this port corrects. **Hard requirements for the port,
stated up front so they can't be quietly traded away again:**

1. Small binary — target low-MB or smaller. NOT tens of MB.
2. Instant launch — no runtime extraction, no first-launch "beat".
3. Single self-contained .exe — no DLLs shipped alongside, no runtime install.
4. If any GUI/toolkit choice threatens 1–3, SAY SO to the user at the decision point with the
   concrete cost, before building. That is the rule that was broken with Avalonia.

## Target stack

- **Language: Rust.** (the user's decision, entry 45.)
- **GUI: `native-windows-gui` (NWG)** + `native-windows-derive`. Thin wrapper over the Win32
  common-control library — uses the OS's own controls, so NO bundled renderer. ~163 KB basic
  app, instantaneous launch, no runtime. NWG is mature/stable ("done") — fine for a small
  stable tool; the risk is it won't gain features, which DirWatch doesn't need.
- **Filesystem watch: the `notify` crate** (the standard Rust FS-watch crate; wraps
  ReadDirectoryChangesW on Windows) PAIRED with the same polling-sweep fallback the .NET
  version uses — do NOT drop the poll; it is the whole reason network shares work (entry 10).
- **Build/target:** `cargo build --release --target x86_64-pc-windows-msvc`. Verify the
  release exe stands ALONE (no sidecar DLLs) — this is the check that would have caught the
  Avalonia problem; make it part of the equivalent of `handoff-checks.sh`.
- Minimal dependencies (collaboration-prompt rule): `nwg`, `notify`, a glob/regex crate for
  the matcher, and an encoding crate (`encoding_rs`) for UTF-16 detection. Justify each.

## What ports mechanically vs. what is a real rewrite

CORE (~900 lines C#, platform-agnostic, unit-testable in the Linux session — port first,
prove parity with unit tests before touching the GUI):

- `GlobMatcher` (59 lines) → straight port. `*.log;*.txt` semantics, case-insensitive.
- `EncodingDetector` (50) → straight port; BOM sniff + UTF-8 default. Use `encoding_rs`.
- `TailReader` (143) → straight port, but MIND the invariant that bit us once: feed bytes to
  a STATEFUL decoder so a multi-byte char split across reads survives (entry 11 regression,
  test `Multibyte_char_split_across_polls`). Rust: keep a partial-byte remainder buffer.
- `WatchService` (202) → the heart. Port the SWEEP-IS-AUTHORITY design exactly: a periodic
  poll diffs a directory listing against last-known (length, mtime, exists) stamps and raises
  Discovered/Activity/Missing/Reappeared; the FS watcher only SCHEDULES debounced sweeps
  (150 ms debounce). Depth-limited manual walk (FSW can only do 0-or-unlimited recursion).
  Files keyed by full path. This maps directly to a Rust struct + `notify` + a timer thread;
  events become channel messages or callbacks the GUI thread drains.
- `SessionModel` (116) → straight port; the button-state state machine (idle/open/
  active-open/unread/missing + active-closed blink). 8 state tests exist — port them.
- `CliOptions` (102) → straight port. Positional dir, `-d`/`--depth`, `-w`/`--max-windows`,
  `--pattern`/`--patterns`, `--poll-ms`, `--active-secs`, `--no-open`, `--version`/`--help`/
  `--license`/`--selftest`. NOTE: the whole WinExe/AttachConsole/CONOUT$ saga (.5–.8) is
  .NET-specific and LARGELY EVAPORATES in Rust — a console-subsystem or dual-mode Rust exe
  prints to the parent console normally. Re-decide subsystem handling fresh; don't port the
  P/Invoke gymnastics. Keep the leading-newline / clean-output behavior (entry 40a) as intent.
- `BuildInfo`/`LicenseInfo` (20/29) → straight port (a `const`/`include_str!`).
- `CoreSelfTest` (191) → port as the `--selftest` smoke test writing `DirWatch_debug.log`.

GUI (~1100 lines Avalonia, the REAL rewrite — no headless test; the user is the eyes):

- `MainWindow` (515) → the big one. Directory box + Browse, patterns box, Depth box,
  Settings/Restart/Start buttons, the per-directory bordered boxes each labeled with the full
  path, the button-per-file grid with the 5 colors + blink, the Open-Windows counter/cap flash,
  auto-grow height per directory box. All of this is Win32 controls in NWG — layout is manual
  (NWG uses explicit layouts, not XAML). Budget the most time here.
- `TailWindow` (86) → a window with a scrolling read-only text view; append-follow; rotation/
  truncation marker; screen-edge-aware first-window placement (entry 33/35d — opens LEFT when
  no room right); Ctrl+W to close; cascade subsequent windows.
- `SettingsWindow` (96) → poll interval, active timeout, max-windows, auto-open. **Build the
  two backlog items INTO this dialog (BACKLOG §0):** P1 a Poll-Interval explanation note, P2 a
  color Legend section. Do these here so they're built once.
- `App`/`Program`/`SelfTest` → Rust `main`, arg dispatch, and the smoke test (the Avalonia
  headless screenshot in `SelfTest.cs` has no Rust equivalent — GUI verification is manual).

## Testing reality (state this to the user early)

- CORE ports are unit-testable in the Linux session with `cargo test` — build parity there
  before shipping anything. This keeps most of the port verifiable off-hardware.
- The NWG GUI compiles only for Windows and cannot be screenshotted on Linux. ALL GUI
  verification is the user's hardware — expect more round-trips than the .NET project had, and a
  test plan that leans on the user's eyes. Set that expectation before the first GUI build.
- Keep the `--selftest` + `<name>_debug.log` + embedded build-ID discipline; keep a
  Rust equivalent of the gate (`cargo fmt --check`, `cargo clippy`, `cargo test`, the
  stands-alone-exe size/sidecar check) as the build gate, and a handoff gate mirroring
  `handoff-checks.sh`.

## Suggested sequence

1. Scaffold the Rust workspace (core lib crate + bin crate), toolchain, gate scripts.
2. Port + unit-test the core (glob, encoding, tail, watch, session, CLI) to green parity.
3. Prove a standalone release exe: build, confirm size target + no sidecar DLLs (the check
   that would have caught Avalonia).
4. Build the GUI incrementally (main window → tail window → settings incl. P1/P2), each
   increment a Windows test round with the user.
5. Reach parity with `.9`, then run the full sign-off.
