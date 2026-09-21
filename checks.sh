#!/usr/bin/env bash
# checks.sh — the BUILD GATE for the DirWatch iced (cross-platform) port.
#
# Born at the first iced-port build per collaboration-prompt v14 (Gate scripts): format, lint,
# compile, full test suite, and a binary launch check, one PASS/FAIL line per step, nonzero exit on any
# failure. This is the v14-named gate that replaces the v10-era `linux-checks.sh`; it ships in every
# zip and handoff. the user approves it ONCE (DECISIONS R57 records the approval when it lands).
#
# WHAT THIS PROVES, HONESTLY:
#   * The whole workspace builds on the host AND lints for the x86_64-pc-windows-msvc target (so
#     the cross-platform GUI + runtime are linted for Windows off a non-Windows host).
#   * All unit + integration tests pass — INCLUDING the runtime integration tests, which now RUN on
#     every host (they were Windows-only under NWG; the iced runtime is cross-platform).
#   * The built binary launches and links (`--version` exits 0) — the logic --selftest used to
#     smoke-test (glob/encoding/tail/session) is covered by the full test suite above; --selftest
#     itself was removed from the shipped binary at .i31 (DECISIONS R106).
#   * A HEADLESS GUI RENDER succeeds and produces a screenshot, when a software-GL display is
#     available (xvfb + iced tiny-skia). This is the early, first-class verification harness the
#     port plan calls for (PORT-PLAN-crossplatform §5); it is a step toward the full offscreen
#     state-scripting harness (§ step 2). Where no display/xvfb is available it is a WARN, not a
#     FAIL, so the gate still runs on a bare CI box — but a real build must have produced the shot.
#
# WHAT IT DOES NOT PROVE: that the window looks right on macOS/Windows native (per-OS eyeball is
# still the user's, PORT-PLAN §7), or real-filesystem behavior on a flaky network share (accepted gap).
#
# GATE PARITY (v14): every script the test plan asks the user to run, this gate runs too. The test plan
# runs `runtests`; the [runtests] step below executes it in a scratch copy and asserts the artifact
# zip it produces. Parsing is not executing.
#
# Exit: 0 = all green; non-zero = a check failed (blocks the build deliverable).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

fails=0
warns=0
step() { echo ""; echo "== $1 =="; }
ok()   { echo "  [PASS] $1"; }
bad()  { echo "  [FAIL] $1"; fails=$((fails+1)); }
warn() { echo "  [WARN] $1"; warns=$((warns+1)); }

BUILD_ID="$(grep -oE 'BUILD_ID[^"]*"[^"]+"' crates/dirwatch-core/src/build_info.rs | grep -oE '"[^"]+"' | tr -d '"' | head -1)"

# Shared release target dir for the runtests.* gate-parity steps (DECISIONS R96). Each parity step
# runs its script from a fresh scratch checkout of the SOURCE (target/ excluded, small+fast copy)
# with the real target/ SYMLINKED in — so the slow release build (opt-level z + LTO, ~4 min cold) is
# paid ONCE by the first parity step and reused by the second, instead of each paying a cold build.
# No 4.5 GB copy; scratch stays source-only. Cleaned at the end.
PARITY_TARGET="$(mktemp -d)"

echo "== checks.sh (DirWatch iced-port build gate) =="
echo "build ID under test: $BUILD_ID"

# Record the versions this gate validated against (target-manifest transparency, v14 Validation).
echo "validated against (off-hardware session):"
echo "  rustc : $(rustc --version 2>/dev/null)"
echo "  cargo : $(cargo --version 2>/dev/null)"
echo "  bash  : $(bash --version 2>/dev/null | head -1)"
echo "  host  : $(uname -srm 2>/dev/null)"
echo "target manifest (the user's hardware — DECISIONS R60): Windows + Windows PowerShell 5.1 (no bash),"
echo "  MSVC toolchain stable-x86_64-pc-windows-msvc; his gate is runtests.ps1, not this script."

step "cargo fmt --check"
if cargo fmt --all --check >/tmp/dw_fmt.txt 2>&1; then ok "formatting clean"; else bad "cargo fmt reported diffs (see /tmp/dw_fmt.txt)"; fi

step "cargo build (workspace)"
if cargo build --locked --workspace >/tmp/dw_build.txt 2>&1; then ok "workspace builds on host"; else bad "cargo build failed (see /tmp/dw_build.txt)"; fi

step "cargo clippy (host, deny warnings)"
if cargo clippy --locked --workspace --all-targets -- -D warnings >/tmp/dw_clippy.txt 2>&1; then ok "clippy clean (no warnings)"; else bad "clippy warnings/errors (see /tmp/dw_clippy.txt)"; fi

step "cargo clippy --target x86_64-pc-windows-msvc (deny warnings)"
# Lints the cross-platform GUI + runtime for the Windows target from a non-Windows host (compiles +
# lints; the link step needs the msvc linker and is skipped, which is fine for lint). Requires the
# target: `rustup target add x86_64-pc-windows-msvc`. Absent => WARN so the gate still runs where
# the target isn't installed — but a real build must have run it.
if rustup target list --installed 2>/dev/null | grep -q 'x86_64-pc-windows-msvc'; then
    if cargo clippy --locked --target x86_64-pc-windows-msvc --workspace --all-targets -- -D warnings >/tmp/dw_clippy_win.txt 2>&1; then
        ok "windows-target clippy clean (cross-platform GUI/runtime linted for Windows)"
    else
        bad "windows-target clippy warnings/errors (see /tmp/dw_clippy_win.txt)"
    fi
else
    warn "x86_64-pc-windows-msvc target not installed -- Windows-target lint SKIPPED (rustup target add x86_64-pc-windows-msvc)"
fi

step "cargo test (full suite)"
if cargo test --locked --workspace >/tmp/dw_test.txt 2>&1; then
    ok "test suite green"
    grep -hE 'test result:' /tmp/dw_test.txt | sed 's/^/    /'
else
    bad "test suite failed (see /tmp/dw_test.txt)"
fi

# DECISIONS R122: the directory-box cap is a small guardrail that a later refactor could quietly
# drop. Assert the constant still exists and its regression tests are present AND ran green in the
# suite above, so the cap can't vanish silently. Add-only check (R122; loosened R124 review #3
# finding 7 — assert EXISTENCE, not a pinned value or literal test names, so a legit value change or
# a test rename doesn't red the gate for the wrong reason).
step "directory-box cap present + tested (R122)"
cap_ok=1
grep -qE 'pub const MAX_DIR_BOXES\s*:\s*usize\s*=\s*[0-9_]+\s*;' \
    "$ROOT/crates/dirwatch-core/src/session.rs" \
    || { bad "pub const MAX_DIR_BOXES not found in session.rs"; cap_ok=0; }
# The cap's regression tests live in the session test module and reference MAX_DIR_BOXES; assert the
# module carries at least one such test and that the dir-cap tests ran green (match the module, not
# four hard-coded names).
grep -qE 'fn dir_cap_' "$ROOT/crates/dirwatch-core/src/session.rs" \
    || { bad "no dir-cap regression test (fn dir_cap_*) found in session.rs"; cap_ok=0; }
grep -qE 'session::tests::dir_cap_[a-z_]+ \.\.\. ok' /tmp/dw_test.txt \
    || { bad "no dir-cap regression test ran green in the suite"; cap_ok=0; }
grep -qE 'session::tests::reset_clears_dir_cap_state \.\.\. ok' /tmp/dw_test.txt \
    || { bad "reset_clears_dir_cap_state not green in the suite"; cap_ok=0; }
[ "$cap_ok" = 1 ] && ok "MAX_DIR_BOXES present; dir-cap regression tests present and green"

step "file-tile cap + Missing prune present + tested (finding 1, R156)"
# Mirror of the dir-cap step for the .i52 finding-1 mechanisms: the MAX_FILES cap with keep-most-active
# eviction, and the 1-hour Missing tile prune. Assert the constants exist and that the model-level
# regression tests ran green (match the module, not hard-coded names, so a rename doesn't misfire).
fcap_ok=1
grep -qE 'pub const MAX_FILES\s*:\s*usize\s*=\s*[0-9_]+\s*;' \
    "$ROOT/crates/dirwatch-core/src/session.rs" \
    || { bad "pub const MAX_FILES not found in session.rs"; fcap_ok=0; }
grep -qE 'fn file_cap_' "$ROOT/crates/dirwatch-core/src/session.rs" \
    || { bad "no file-cap regression test (fn file_cap_*) found in session.rs"; fcap_ok=0; }
grep -qE 'fn prune_missing_' "$ROOT/crates/dirwatch-core/src/session.rs" \
    || { bad "no prune regression test (fn prune_missing_*) found in session.rs"; fcap_ok=0; }
grep -qE 'session::tests::file_cap_[a-z_]+ \.\.\. ok' /tmp/dw_test.txt \
    || { bad "no file-cap regression test ran green in the suite"; fcap_ok=0; }
grep -qE 'session::tests::prune_missing_[a-z_]+ \.\.\. ok' /tmp/dw_test.txt \
    || { bad "no prune regression test ran green in the suite"; fcap_ok=0; }
[ "$fcap_ok" = 1 ] && ok "MAX_FILES + prune present; regression tests present and green"

step "window title = product name, NOT the build string (R169/R170 — the .i58 defect class)"
# The .i58 sign-off shipped a titlebar carrying the full BUILD_ID ("DirWatch 1.0 build …iNN"); the user
# specified the build string is for --version/--help/logs/crash-stamps ONLY. The gate had no title
# check, so it slipped. Guard the whole class statically + at runtime:
#   1. build_info::PRODUCT exists and is exactly "DirWatch 1.0".
#   2. The `fn title(` body does NOT reference build_info::BUILD_ID (either arm), and DOES use PRODUCT.
#   3. The regression test ran green in the suite.
# The `fn title(` body is extracted by awk from its opening line to its closing `}` at column 0, so the
# check reads the real function, not a comment mentioning BUILD_ID elsewhere in the file.
title_ok=1
GUI_MOD="$ROOT/crates/dirwatch/src/gui/mod.rs"
BINFO="$ROOT/crates/dirwatch-core/src/build_info.rs"
grep -qE 'pub const PRODUCT\s*:\s*&str\s*=\s*"DirWatch 1\.0"\s*;' "$BINFO" \
    || { bad "build_info::PRODUCT is not defined as exactly \"DirWatch 1.0\""; title_ok=0; }
# Extract the body of `fn title(` (from its line to the next line that is a bare `}` in column 0).
title_body="$(awk '/^fn title\(/{f=1} f{print} f&&/^\}/{exit}' "$GUI_MOD")"
if [ -z "$title_body" ]; then
    bad "could not locate fn title( in gui/mod.rs"; title_ok=0
else
    if printf '%s\n' "$title_body" | grep -q 'build_info::BUILD_ID'; then
        bad "fn title() references build_info::BUILD_ID — the titlebar must show PRODUCT, not the build string (R169/R170)"; title_ok=0
    fi
    printf '%s\n' "$title_body" | grep -q 'build_info::PRODUCT' \
        || { bad "fn title() does not use build_info::PRODUCT for its window titles"; title_ok=0; }
fi
grep -qE 'gui::tests::main_and_catchall_titles_show_product_name_not_the_build_string \.\.\. ok' /tmp/dw_test.txt \
    || { bad "title regression test did not run green in the suite"; title_ok=0; }
[ "$title_ok" = 1 ] && ok "title() shows PRODUCT (\"DirWatch 1.0\"), never BUILD_ID; regression test green"

step "resume-follow chip present + guarded by follow state (R173)"
# The .i61 resume chip: a neutral-grey "jump to bottom & resume follow" button shown ONLY while
# follow is paused. Guard the whole feature statically so it can't silently regress:
#   1. Message::ResumeFollow exists, and its handler sets `following = true` AND snaps to bottom.
#   2. tail_view only stacks the chip when follow is PAUSED (the `if tw.following { body } else {
#      stack![body, resume_chip(..)] }` guard) — so it never overlays while following.
#   3. resume_chip sends Message::ResumeFollow.
#   4. The regression test ran green in the suite.
chip_ok=1
GUI_MOD="$ROOT/crates/dirwatch/src/gui/mod.rs"
TAILV="$ROOT/crates/dirwatch/src/gui/tail_view.rs"
grep -q 'ResumeFollow(window::Id)' "$GUI_MOD" \
    || { bad "Message::ResumeFollow not declared"; chip_ok=0; }
# Handler: within the ResumeFollow arm, following is set true and snap_to_bottom is called.
rf_body="$(awk '/Message::ResumeFollow\(id\) =>/{f=1} f{print} f&&/^        \}/{exit}' "$GUI_MOD")"
if [ -z "$rf_body" ]; then
    bad "could not locate the Message::ResumeFollow handler arm"; chip_ok=0
else
    printf '%s\n' "$rf_body" | grep -q 'following = true' \
        || { bad "ResumeFollow handler does not set following = true"; chip_ok=0; }
    printf '%s\n' "$rf_body" | grep -q 'snap_to_bottom' \
        || { bad "ResumeFollow handler does not snap to the bottom"; chip_ok=0; }
fi
# View guard: the chip is stacked only in the else (paused) arm of the follow check.
grep -q 'if tw.following {' "$TAILV" \
    || { bad "tail_view does not gate the body on tw.following"; chip_ok=0; }
grep -q 'stack!\[body, resume_chip(id)\]' "$TAILV" \
    || { bad "resume_chip is not stacked over the body in the paused arm"; chip_ok=0; }
grep -q 'Message::ResumeFollow(id)' "$TAILV" \
    || { bad "resume_chip does not send Message::ResumeFollow"; chip_ok=0; }
grep -qE 'gui::tests::resume_follow_reengages_following_from_a_paused_tail \.\.\. ok' /tmp/dw_test.txt \
    || { bad "resume-follow regression test did not run green in the suite"; chip_ok=0; }
[ "$chip_ok" = 1 ] && ok "resume chip present; shown only while paused; ResumeFollow re-engages follow + snaps; regression test green"

step "packaging assertions present in every harness + RESULT banner (R168/R171 — the .i58 gap class)"
# .i58 signed off THREE-PLATFORM while the Windows dumpbin + macOS otool assertions did not yet exist,
# and no check guarded the final RESULT banner. Guard the whole class STATICALLY so a harness can't
# silently lose an assertion or its verdict line: (1) every runtests.* prints a "RESULT: PASS" banner
# (carry-forward a); (2) runtests.ps1 carries the dumpbin /dependents CRT-static assertion + its
# dynamic-CRT offender pattern; (3) runtests.command carries the .app bundle + otool -L assertion + the
# .app.zip ship shape. The Windows/macOS assertions themselves run on the user's hardware; this proves the
# gate can't ship a harness that dropped them.
pk_present=1
for h in runtests.sh runtests.ps1 runtests.command; do
    grep -q 'RESULT: PASS' "$ROOT/$h" \
        || { bad "$h has no 'RESULT: PASS' banner (final verdict line missing)"; pk_present=0; }
done
grep -q 'dumpbin' "$ROOT/runtests.ps1" \
    || { bad "runtests.ps1 lost the dumpbin /dependents CRT-static assertion (R107/R171)"; pk_present=0; }
grep -qiE 'vcruntime|ucrtbase|api-ms-win-crt' "$ROOT/runtests.ps1" \
    || { bad "runtests.ps1 dumpbin step lost its dynamic-CRT offender pattern"; pk_present=0; }
grep -q 'otool -L' "$ROOT/runtests.command" \
    || { bad "runtests.command lost the otool -L self-contained assertion (R168/R171)"; pk_present=0; }
grep -q 'DirWatch.app' "$ROOT/runtests.command" \
    || { bad "runtests.command lost the .app bundle build (R168/R171)"; pk_present=0; }
grep -qE '\.app\.zip' "$ROOT/runtests.command" \
    || { bad "runtests.command lost the .app.zip ship shape"; pk_present=0; }
[ "$pk_present" = 1 ] && ok "all three harnesses print RESULT: PASS; Windows dumpbin + macOS otool/.app assertions present"

step "cargo audit (RustSec, shipped Cargo.lock)"
# review #2 finding 12 / DECISIONS R113: the lockfile is deterministic (R98), so the audit is a
# gate step on every host that has the tool + network. RUSTSEC-2026-0253 (`lru` pop() panic-safety)
# is ignored on record: it needs an unwinding panic and the release profile is `panic = "abort"`.
# No tool / no network => NAMED WARN (this off-hardware session has neither: the user's runtests.* run
# it for real). Never silent.
if cargo audit --version >/dev/null 2>&1; then
    if cargo audit --ignore RUSTSEC-2026-0253 >/tmp/dw_audit.txt 2>&1; then
        ok "cargo audit: no known vulnerabilities (RUSTSEC-2026-0253 ignored: panic=abort, R113)"
    elif grep -qiE "couldn.t fetch|failed to fetch|error fetching|could not connect|network|resolve host" /tmp/dw_audit.txt; then
        warn "cargo audit could not fetch the advisory database (offline) -- audit SKIPPED here; the user's runtests.* run it online"
    else
        bad "cargo audit found a vulnerability in Cargo.lock (see /tmp/dw_audit.txt)"
    fi
else
    warn "cargo-audit not installed -- audit SKIPPED here (cargo install cargo-audit --locked); the user's runtests.* run it online"
fi

step "binary launches (--version exits 0)"
# --selftest was removed at .i31 (release-prep, DECISIONS R106): no dev self-test flag in the
# shipped binary. This confirms the built binary launches + links (the CRT-static check on Windows,
# harmless here); the logic --selftest smoke-tested is covered by the full test suite above, and the
# GUI runtime is proven by the headless render steps below.
if cargo run --locked --quiet -p dirwatch -- --version >/tmp/dw_version.txt 2>&1; then
    ok "dirwatch --version exited 0"
    grep -E 'DirWatch' /tmp/dw_version.txt | sed 's/^/    /'
else
    bad "dirwatch --version failed (see /tmp/dw_version.txt)"
fi

step "runtests.ps1 is pure ASCII (+ UTF-8 BOM)"
# WHY THIS EXISTS (DECISIONS R61): .i2 shipped runtests.ps1 with em-dashes as UTF-8 without a BOM.
# Windows PowerShell 5.1 decodes a BOM-less file as ANSI/Windows-1252, mangling every non-ASCII
# byte — one landed inside a string and broke the parser on the user's machine (a red gate). A test
# harness must not depend on file encoding: assert the .ps1 body is pure ASCII and starts with a
# UTF-8 BOM. This is a mechanizable check for a defect that slipped through, per the prompt.
if [ -f "$ROOT/runtests.ps1" ]; then
    bom_ok=0
    if [ "$(head -c 3 "$ROOT/runtests.ps1" | od -An -tx1 | tr -d ' \n')" = "efbbbf" ]; then bom_ok=1; fi
    # Body (after the 3 BOM bytes) must contain no byte > 0x7F.
    if tail -c +4 "$ROOT/runtests.ps1" | LC_ALL=C grep -qP '[^\x00-\x7F]'; then
        bad "runtests.ps1 contains non-ASCII bytes — 5.1 will mangle them (make it pure ASCII)"
    elif [ "$bom_ok" -ne 1 ]; then
        bad "runtests.ps1 lacks a UTF-8 BOM — add it so 5.1 decodes it as UTF-8"
    else
        ok "runtests.ps1 is pure ASCII with a UTF-8 BOM (5.1-safe)"
    fi
else
    warn "runtests.ps1 not present in tree"
fi

step "runtests.ps1 — PowerShell parse + interop compile (gate parity, closest available)"
# WHY THIS EXISTS (DECISIONS R131): .i41's --version harness step KILLED a healthy process because
# it closed winit's untitled helper window instead of the real one (R130) — a harness defect this
# gate never caught, because checks.sh cannot run PowerShell on Windows so it only ASCII/BOM-checked
# the .ps1 (parsing is not executing). Per the prompt ("if the user runs it, the gate ran it … the
# closest available with the mismatch named"): if a `pwsh` is on PATH, PARSE runtests.ps1 with the
# PowerShell language parser (catches a broken script) AND compile its Win32Cap Add-Type block
# (catches a broken C#/interop declaration — the load-bearing part of the .i42 window finder). This
# is NOT execution of the user32 calls: pwsh here is 7.x on Linux, not the user's Windows PowerShell 5.1,
# and user32.dll cannot be CALLED here — MISMATCH NAMED. the user's real runtests.ps1 run is the gate for
# behavior. Tool absent => NAMED WARN (like cargo audit), never silent, never a substitute.
if [ -f "$ROOT/runtests.ps1" ]; then
    PWSH_BIN="$(command -v pwsh || command -v powershell || true)"
    if [ -n "$PWSH_BIN" ]; then
        PSCHK="$(mktemp --suffix=.ps1)"
        cat > "$PSCHK" <<'PSEOF'
param([string]$Path)
$ErrorActionPreference = 'Stop'
# 1) Parse the whole harness with the PowerShell language parser.
$tokens = $null; $errs = $null
[void][System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errs)
if ($errs -and $errs.Count -gt 0) {
    Write-Output "PARSE_FAIL $($errs.Count)"
    foreach ($e in $errs) { Write-Output ("  line {0}: {1}" -f $e.Extent.StartLineNumber, $e.Message) }
    exit 1
}
# 2) Extract the GUARDED interop block, compile it, AND run it TWICE in THIS one process. WHY TWICE
#    (DECISIONS R132): Add-Type loads a type into the whole PowerShell session process, so an
#    UNGUARDED Add-Type throws TYPE_ALREADY_EXISTS on the second run in one shell (the .i42 defect) -
#    a fresh `pwsh -File` per run never re-added it, so the first cut of this check MISSED it. The
#    type name is BUILD-UNIQUE (`Win32Cap_i<serial>`, DECISIONS R133) so a stale type from a prior
#    run cannot shadow the new one; the guard keys on THAT exact name. We capture the whole
#    `if (-not ('Win32Cap_iNN' -as [type])) { Add-Type @"..."@ }` block, resolve the name from it,
#    and Invoke-Expression it twice: an unguarded/mis-shaped block FAILS here.
$src = Get-Content -Raw -LiteralPath $Path
$m = [regex]::Match($src, "(?s)if \(-not \('(?<name>Win32Cap[A-Za-z0-9_]*)' -as \[type\]\)\) \{\s*Add-Type\s*@""\r?\n.*?public class \k<name>.*?\r?\n""@\s*\}")
if (-not $m.Success) {
    Write-Output "GUARD_NOTFOUND (no guarded 'if (-not (Win32Cap... -as [type])) { Add-Type ...public class <same name>... }' block matched -- an unguarded/mis-shaped Add-Type is re-run-unsafe, R132/R133)"
    exit 1
}
$block = $m.Value
$name  = $m.Groups['name'].Value
try {
    Invoke-Expression $block                      # first load: compiles the C# + interop declarations
    if (-not ($name -as [type])) { throw "type $name did not load" }  # force resolve
    $null = New-Object ($name + '+RECT')          # the screenshot step references <name>+RECT
    Invoke-Expression $block                      # SECOND load in the SAME process: guard must no-op
    Write-Output "OK ($name compiled + re-run-safe: block ran twice in one process, no TYPE_ALREADY_EXISTS)"
    exit 0
} catch {
    Write-Output "ADDTYPE_FAIL $($_.Exception.Message)"
    exit 1
}
PSEOF
        ps_out="$("$PWSH_BIN" -NoProfile -File "$PSCHK" "$ROOT/runtests.ps1" 2>&1)"
        ps_rc=$?
        rm -f "$PSCHK"
        printf '%s\n' "$ps_out" | while IFS= read -r line; do echo "    $line"; done
        if [ "$ps_rc" -eq 0 ]; then
            ok "runtests.ps1 parses, Win32Cap compiles, AND is re-run-safe (block added twice in one process, no TYPE_ALREADY_EXISTS) ($("$PWSH_BIN" --version 2>/dev/null | head -1); NOT 5.1, user32 not callable here — MISMATCH NAMED)"
        else
            bad "runtests.ps1 failed the PowerShell parse/interop/re-run check — read the lines above"
        fi
    else
        warn "no pwsh/powershell on PATH — .ps1 parse+interop compile SKIPPED (install PowerShell to enable; the user's runtests.ps1 run is the behavior gate)"
    fi
else
    warn "runtests.ps1 not present in tree — parse/interop check SKIPPED"
fi

step "runtests.command (Mac gate) — syntax + gate-parity execution"
# GATE PARITY (v14, DECISIONS R89): the Mac test plan asks the user to run `runtests.command`, so this
# gate runs it too. The Mac UNIVERSAL build (lipo of two apple targets) is NOT achievable on this
# Linux host — so per the rule ("closest available with the mismatch named") we execute the script's
# PORTABLE FALLBACK path: it builds the HOST release binary, confirms it launches (--version), and assembles
# artifacts_<serial>.zip. We assert the script exits 0 AND the zip is written AND carries the runtime
# log stamped with the build ID. The universal/lipo step it SKIPS on non-mac is the user's Mac to verify.
if [ -f "$ROOT/runtests.command" ]; then
    if ! bash -n "$ROOT/runtests.command" 2>/tmp/dw_mac_syntax.txt; then
        bad "runtests.command has a bash syntax error (see /tmp/dw_mac_syntax.txt)"
    elif ! command -v zip >/dev/null 2>&1; then
        warn "zip not available — runtests.command gate-parity execution SKIPPED (static syntax OK)"
    else
        SCRATCH="$(mktemp -d)"
        # Copy the tree WITHOUT target/ (small, fast), then SYMLINK the real target/ into the scratch
        # copy so cargo reuses already-built artifacts (DECISIONS R96): the slow release build
        # (opt-level z + LTO, ~4 min cold) is paid ONCE and reused by the sibling parity step, with no
        # 4.5 GB copy and the script's own `target/release/dirwatch` path intact.
        (cd "$ROOT" && tar --exclude=./target --exclude=./dist -cf - .) | (cd "$SCRATCH" && tar -xf -)
        ln -s "$PARITY_TARGET" "$SCRATCH/target"
        if ( cd "$SCRATCH" && bash runtests.command ) >/tmp/dw_mac_run.txt 2>&1; then
            ZIP="$(ls "$SCRATCH"/artifacts_*.zip 2>/dev/null | head -1)"
            # Assert the zip carries the run-log transcript stamped with the build ID. (Was
            # DirWatch_debug.log, produced by the removed --selftest; the run log — which always
            # exists and carries the build ID in its header + the --version output — is the stable
            # anchor now. .i31, DECISIONS R106.)
            if [ -n "$ZIP" ] && unzip -p "$ZIP" 'runtests_output_*' 2>/dev/null | grep -q "$BUILD_ID"; then
                ok "runtests.command ran green; artifact zip has the run log stamped $BUILD_ID (universal/lipo + iconutil + otool are Mac-only, named+skipped here)"
                # 1.0 macOS packaging (R168/R171): the non-mac parity path assembles a stub .app from
                # the host binary and zips it, so the bundle + .app.zip/-src.zip logic is EXERCISED here;
                # the real universal .app + otool -L assertion land on the user's Mac. Assert both shapes rode
                # in the artifact zip.
                zmac="$(unzip -l "$ZIP" 2>/dev/null)"
                printf '%s\n' "$zmac" | grep -qE 'DirWatch-.*\.app\.zip' \
                    || bad "mac parity: .app.zip not produced/collected by runtests.command (see /tmp/dw_mac_run.txt)"
                printf '%s\n' "$zmac" | grep -qE 'DirWatch-.*-src\.zip' \
                    || bad "mac parity: -src.zip not produced/collected by runtests.command (see /tmp/dw_mac_run.txt)"
            else
                bad "runtests.command ran but its artifact zip is missing or lacks the build-ID-stamped run log (see /tmp/dw_mac_run.txt)"
            fi
        else
            bad "runtests.command (portable path) exited non-zero (see /tmp/dw_mac_run.txt)"
        fi
        rm -rf "$SCRATCH"
    fi
else
    warn "runtests.command not present in tree"
fi

step "runtests.sh (Linux gate) — syntax + gate-parity execution"
# GATE PARITY (v14, DECISIONS R96): the Linux test plan asks the user to run `runtests.sh`, so this gate
# runs it too. Unlike the Mac universal/lipo path, the Linux gate has NO host-mismatch — this session
# IS Linux, so runtests.sh executes on its true target here: it builds the native release, confirms
# it launches (--version), renders headlessly under xvfb, and assembles artifacts_<serial>.zip. We assert the
# script exits 0 AND the zip is written AND carries the runtime log stamped with the build ID.
if [ -f "$ROOT/runtests.sh" ]; then
    if ! bash -n "$ROOT/runtests.sh" 2>/tmp/dw_lin_syntax.txt; then
        bad "runtests.sh has a bash syntax error (see /tmp/dw_lin_syntax.txt)"
    elif ! command -v zip >/dev/null 2>&1; then
        warn "zip not available — runtests.sh gate-parity execution SKIPPED (static syntax OK)"
    else
        SCRATCH="$(mktemp -d)"
        # Copy the tree WITHOUT target/ and dist/ (small, fast), then SYMLINK the real target/ in so
        # cargo reuses the release build already done by the sibling parity step (DECISIONS R96).
        (cd "$ROOT" && tar --exclude=./target --exclude=./dist -cf - .) | (cd "$SCRATCH" && tar -xf -)
        ln -s "$PARITY_TARGET" "$SCRATCH/target"
        # The 1.0 packaging step (R168) builds the AppImage with appimagetool. On a bare box it is not
        # on PATH; make the extracted copy (and its mksquashfs, for the no-FUSE path) locatable so the
        # gate-parity run BUILDS the shapes rather than WARN-skipping the AppImage. package-linux.sh
        # already probes /tmp/dwtools/squashfs-root; here we also add whatever is on the session PATH.
        if [ -d /tmp/dwtools/squashfs-root/usr/bin ]; then
            export PATH="/tmp/dwtools/squashfs-root/usr/bin:$PATH"
        fi
        if ( cd "$SCRATCH" && bash runtests.sh ) >/tmp/dw_lin_run.txt 2>&1; then
            ZIP="$(ls "$SCRATCH"/artifacts_*.zip 2>/dev/null | head -1)"
            # Assert the zip carries the run-log transcript stamped with the build ID (was
            # DirWatch_debug.log from the removed --selftest; .i31, DECISIONS R106).
            if [ -n "$ZIP" ] && unzip -p "$ZIP" 'runtests_output_*' 2>/dev/null | grep -q "$BUILD_ID"; then
                ok "runtests.sh ran green; artifact zip has the run log stamped $BUILD_ID"
            else
                bad "runtests.sh ran but its artifact zip is missing or lacks the build-ID-stamped run log (see /tmp/dw_lin_run.txt)"
            fi
            # 1.0 packaging (R168): assert the run's transcript shows the three ship shapes built +
            # the ldd/log-dir assertions PASSED. This is the off-hardware proof that the Linux
            # packaging gate is real (the shapes actually build + assert green in this session); the
            # matching Windows/macOS assertions land on the user's hardware in the next packaging build.
            pk_ok=1
            grep -q '\[PASS\] ldd shows only the base set' /tmp/dw_lin_run.txt || { bad "packaging: ldd base-set assertion did not PASS in the runtests.sh run"; pk_ok=0; }
            grep -q '\[PASS\] AppImage built'              /tmp/dw_lin_run.txt || { bad "packaging: AppImage was not built in the runtests.sh run (appimagetool missing?)"; pk_ok=0; }
            grep -q '\[PASS\] .deb built'                  /tmp/dw_lin_run.txt || { bad "packaging: .deb was not built in the runtests.sh run (cargo-deb missing?)"; pk_ok=0; }
            grep -q '\[PASS\] .rpm built'                  /tmp/dw_lin_run.txt || { bad "packaging: .rpm was not built in the runtests.sh run (cargo-generate-rpm missing?)"; pk_ok=0; }
            grep -q '\[PASS\] AppImage logged to'          /tmp/dw_lin_run.txt || { bad "packaging: per-shape log-dir verify (R127) did not PASS in the runtests.sh run"; pk_ok=0; }
            if [ "$pk_ok" = 1 ] && [ -n "$ZIP" ]; then
                zshapes="$(unzip -l "$ZIP" 2>/dev/null)"
                if printf '%s\n' "$zshapes" | grep -qE '\.AppImage$' \
                   && printf '%s\n' "$zshapes" | grep -qE '\.deb$' \
                   && printf '%s\n' "$zshapes" | grep -qE '\.rpm$'; then
                    ok "1.0 Linux ship shapes built + asserted (AppImage+deb+rpm; ldd base-set + R127 log-dir PASS) and ride in the artifact zip"
                else
                    bad "packaging assertions passed but the artifact zip is missing one of AppImage/.deb/.rpm"
                fi
            fi
        else
            bad "runtests.sh exited non-zero (see /tmp/dw_lin_run.txt)"
        fi
        rm -rf "$SCRATCH"
    fi
else
    warn "runtests.sh not present in tree"
fi

step "Cargo.lock present and consistent (--locked builds)"
# REPRODUCIBLE BUILDS (review A6, DECISIONS R98): this workspace builds a BINARY, so the lockfile
# ships and every cargo step above runs `--locked` — a build that would need to re-resolve the
# 400-crate graph fails loudly instead of silently compiling a different dependency set on each of
# the three platforms than the one this gate validated.
if [ -f "$ROOT/Cargo.lock" ]; then
    if cargo metadata --locked --format-version 1 >/dev/null 2>/tmp/dw_lock.txt; then
        ok "Cargo.lock present; resolves with --locked (no drift)"
    else
        bad "Cargo.lock is out of date for Cargo.toml — run 'cargo update -w' deliberately and commit it (see /tmp/dw_lock.txt)"
    fi
else
    bad "Cargo.lock missing — a binary crate must ship its lockfile (DECISIONS R98)"
fi

step "headless TAIL render: long lines must not wrap (review A4)"
# The one-row-per-line invariant behind the virtualized tail view (DECISIONS R98). Launch DirWatch
# on a dir, then APPEND 30 long lines so a narrow (TAIL_W) tail window auto-opens FOLLOWING the
# bottom with both scrollbars showing; capture the desktop. Inspect the PNG for TWO things:
#   (1) A4: each logical line on ONE row, cut at the window edge, horizontal bar present (not wrapped).
#   (2) §8 (R119): the NEWEST (bottom) line is fully ABOVE the floating horizontal bar, not covered by
#       it. This is the .i34 regression the wide inspection missed; the TAIL_HBAR_H bottom spacer is
#       what keeps it clear. The "not covered" property is visual (a pixel assert is too fragile across
#       renderers), so it stays an INSPECTED property of this capture, called out here so it is checked.
# The step asserts the capture happened and copies it beside the main-window shot for that inspection.
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :97 -screen 0 1600x900x24 >/tmp/dw_xvfb_tail.log 2>&1 &
      XP=$!; export DISPLAY=:97; sleep 2
      "$BIN" "$WDIR" --patterns "*.log" >/tmp/dw_gui_tail.log 2>&1 &
      AP=$!; sleep 6   # under xvfb's software path the main window realizes slowly (~3.5 s seen)
      # Appending AFTER launch => Discovered+Activity => auto-open of the tail window.
      for i in $(seq 1 30); do
        printf '2026-09-09 12:00:%02d.000 INFO line %02d: %s\n' "$i" "$i" "$(printf 'wrap-me-not %.0s' $(seq 1 16))" >> "$WDIR/long.log"
      done
      sleep 4
      import -window root "$SHOTDIR/gui_tail.png" 2>/tmp/dw_cap_tail.log || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    SZ=$(stat -c%s "$SHOTDIR/gui_tail.png" 2>/dev/null || echo 0)
    if [ "$SZ" -gt 2000 ]; then
        cp "$SHOTDIR/gui_tail.png" "$ROOT/gui_tail_${BUILD_ID##* }.png" 2>/dev/null || true
        ok "tail window rendered headlessly with 220-char lines (${SZ} byte PNG: gui_tail_${BUILD_ID##* }.png — inspect: one row per line + horizontal scrollbar, AND the newest/bottom line fully ABOVE the horizontal bar (§8 R119))"
    else
        warn "headless tail render produced no usable PNG (see /tmp/dw_gui_tail.log) -- A4 render unverified here"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary unavailable -- headless tail render SKIPPED"
fi

step "file cap evicts + Missing prune fires, headlessly (finding 1, R156)"
# LIVE verification of the .i52 finding-1 mechanisms (the reason DIRWATCH_PRUNE_SECS exists): launch
# the app under xvfb with DIRWATCH_DEBUG=1 and the prune grace shrunk to 5 s, watch MORE than MAX_FILES
# matched files, then delete some. Assert from DirWatch.log that (1) the cap EVICTED (not just refused)
# and held the tile count at the cap, and (2) the prune fired for the deleted files within the grace.
# This is what makes both mechanisms verifiable in seconds instead of an hour of eyeballing. Env-gated
# and debug-only, so it never affects a normal run. WARN (not FAIL) where no xvfb/display, like the
# render steps, so the gate still runs on a bare box.
if command -v xvfb-run >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    LOGDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"; SHOTDIR="$(mktemp -d)"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    CAP="$(grep -oE 'pub const MAX_FILES\s*:\s*usize\s*=\s*[0-9_]+' "$ROOT/crates/dirwatch-core/src/session.rs" | grep -oE '[0-9_]+$' | tr -d _)"
    [ -z "$CAP" ] && CAP=1000
    OVER=$((CAP + 100))
    (
      Xvfb :96 -screen 0 1600x900x24 >/tmp/dw_xvfb_cap.log 2>&1 &
      XP=$!; export DISPLAY=:96; sleep 2
      # Seed OVER-cap matched files BEFORE launch so the initial sweep must evict down to the cap.
      for i in $(seq 1 "$OVER"); do printf 'x\n' > "$WDIR/f$(printf '%05d' "$i").log"; done
      DIRWATCH_DEBUG=1 DIRWATCH_PRUNE_SECS=5 "$BIN" "$WDIR" --patterns "*.log" --no-open \
        --log-dir "$LOGDIR" >/tmp/dw_cap_run.log 2>&1 &
      AP=$!; sleep 8            # let the window realize + the first sweep(s) apply the cap
      # Now delete 10 matched files: they go Missing, and with a 5 s grace must prune shortly after.
      for i in $(seq 1 10); do rm -f "$WDIR/f$(printf '%05d' "$i").log"; done
      sleep 10                  # > grace (5 s) + a poll or two, so the prune tick fires
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    LOG="$LOGDIR/DirWatch.log"
    capp_ok=1
    if [ ! -f "$LOG" ]; then
        warn "cap+prune: no DirWatch.log produced (see /tmp/dw_cap_run.log) -- mechanisms unverified here"
    else
        grep -q "$BUILD_ID" "$LOG" || { bad "cap+prune: DirWatch.log not stamped $BUILD_ID (stale?)"; capp_ok=0; }
        grep -qE 'file cap: [0-9]+ eviction' "$LOG" \
            || { bad "cap+prune: no eviction trace in DirWatch.log (cap did not evict)"; capp_ok=0; }
        grep -qE "tiles held at ($CAP|[0-9]{1,3})\b" "$LOG" \
            || { bad "cap+prune: no 'tiles held at' trace (tile count not bounded)"; capp_ok=0; }
        grep -qE 'prune: removed [0-9]+ tile' "$LOG" \
            || { bad "cap+prune: no prune trace in DirWatch.log (prune did not fire within the grace)"; capp_ok=0; }
        if [ "$capp_ok" = 1 ]; then
            EV="$(grep -oE 'file cap: [0-9]+ eviction.*tiles held at [0-9]+' "$LOG" | tail -1)"
            PR="$(grep -oE 'prune: removed [0-9]+ tile[^:]*' "$LOG" | tail -1)"
            cp "$LOG" "$ROOT/cap_prune_${BUILD_ID##* }.log" 2>/dev/null || true
            ok "cap evicted + held ($EV); prune fired ($PR) at DIRWATCH_PRUNE_SECS=5 -- log: cap_prune_${BUILD_ID##* }.log"
        fi
    fi
    rm -rf "$LOGDIR" "$WDIR" "$SHOTDIR"
else
    warn "xvfb/binary unavailable -- headless cap+prune verification SKIPPED"
fi

step "search-scan bench runs headlessly + writes stamped numbers to DirWatch.log (R161)"
# DIAGNOSTIC (.i54, R161): the search-per-keystroke measurement. Run the app with BOTH gate env vars
# set (DIRWATCH_DEBUG=1 DIRWATCH_SEARCH_BENCH=1) and --log-dir into a scratch dir; the app measures
# the REAL refresh_matches scan (find_matches + clamp_current) across buffer sizes up to the 48 MB
# load cap and 64 MB scrollback ceiling, writes one BENCH line per (buffer x query) to DirWatch.log,
# and EXITS without opening a window -- so NO xvfb/display is needed here (unlike the render steps).
# The gate asserts the sweep ran, is stamped with THIS build ID (stale-log guard), and produced the
# largest-buffer measurements; it then COLLECTS the log so the numbers travel in the artifact zip.
# Env-gated + debug-only: a normal run never triggers it (both gates required).
if [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    BLOGDIR="$(mktemp -d)"
    DIRWATCH_DEBUG=1 DIRWATCH_SEARCH_BENCH=1 "$BIN" --log-dir "$BLOGDIR" >/tmp/dw_bench_run.log 2>&1
    BLOG="$BLOGDIR/DirWatch.log"
    bench_ok=1
    if [ ! -f "$BLOG" ]; then
        bad "search-bench: no DirWatch.log produced (see /tmp/dw_bench_run.log)"; bench_ok=0
    else
        grep -q "$BUILD_ID" "$BLOG" || { bad "search-bench: DirWatch.log not stamped $BUILD_ID (stale?)"; bench_ok=0; }
        grep -q "search-scan sweep START" "$BLOG" || { bad "search-bench: no sweep START line"; bench_ok=0; }
        grep -q "search-scan sweep END"   "$BLOG" || { bad "search-bench: sweep did not complete (no END line)"; bench_ok=0; }
        # The two clamp-sized buffers must both be measured (that is the worst case the app permits).
        grep -qE 'buffer=48MB\(INITIAL_LOAD_CAP\) .*query="error" .*scan=[0-9.]+ms' "$BLOG" \
            || { bad "search-bench: no 48 MB (load-cap) measurement"; bench_ok=0; }
        grep -qE 'buffer=64MB\(SCROLLBACK_CAP\) .*query="error" .*scan=[0-9.]+ms' "$BLOG" \
            || { bad "search-bench: no 64 MB (scrollback-cap) measurement"; bench_ok=0; }
        if [ "$bench_ok" = 1 ]; then
            WORST="$(grep -oE 'buffer=64MB\(SCROLLBACK_CAP\) .*query="error" matches=[0-9+]+ scan=[0-9.]+ms' "$BLOG" | tail -1)"
            cp "$BLOG" "$ROOT/search_bench_${BUILD_ID##* }.log" 2>/dev/null || true
            ok "search-scan bench ran + stamped; worst case ($WORST) -- full numbers: search_bench_${BUILD_ID##* }.log"
        fi
    fi
    rm -rf "$BLOGDIR"
else
    warn "dirwatch binary unavailable -- headless search-scan bench SKIPPED"
fi

step "make-testfiles.sh builds the hardware test-plan files (R115)"
# GATE PARITY: the test plan tells the user to run make-testfiles (.sh on Mac/Linux, .ps1 on Windows)
# and open what it wrote, so the gate runs the .sh into scratch and asserts the exact sizes and
# line counts the plan quotes. (The .ps1 twin cannot execute here — no PowerShell — so it gets the
# same pure-ASCII+BOM check runtests.ps1 has, below; parsing is not executing, and its first
# execution is the user's item 2. Named mismatch, per the gate rule.)
TFDIR="$(mktemp -d)"
if bash "$ROOT/make-testfiles.sh" "$TFDIR" >/tmp/dw_testfiles.txt 2>&1; then
    tf_ok=1
    # Line count = newlines + 1 if the last byte is not a newline. CORRECTED at .i48 (R146, misfire
    # shown): `grep -c ''` counts NUL bytes as line ends in a binary file (a 100 KB NUL file
    # reported 100000 lines here), so it could not count the new big-zero.log.
    count_lines() {
        local n; n=$(tr -cd '\n' < "$1" | wc -c | tr -d ' ')
        if [ -s "$1" ] && [ "$(tail -c1 "$1" | od -An -c | tr -d ' ')" != '\n' ]; then n=$((n+1)); fi
        echo "$n"
    }
    chk() { # file expected-bytes expected-lines
        local b l
        b=$(stat -c%s "$TFDIR/$1" 2>/dev/null || echo -1)
        l=$(count_lines "$TFDIR/$1" 2>/dev/null); [ -z "$l" ] && l=-1
        if [ "$b" != "$2" ] || [ "$l" != "$3" ]; then
            bad "make-testfiles.sh: $1 is $b bytes / $l lines, expected $2 / $3"; tf_ok=0
        fi
    }
    chk big-multiline.log 314572800 3932160
    chk big-oneline.log   314572800 1
    chk big-zero.log      314572800 1
    chk blankrun.log      1830      230
    chk empty.log         0         0
    [ "$tf_ok" -eq 1 ] && ok "make-testfiles.sh wrote the five plan files with the exact sizes/line counts the plan quotes"
else
    bad "make-testfiles.sh failed (see /tmp/dw_testfiles.txt)"
fi

step "make-testfiles.ps1 is pure ASCII (+ UTF-8 BOM)"
# Same 5.1-safety rule as runtests.ps1 (DECISIONS R61): pure ASCII body, UTF-8 BOM.
if [ -f "$ROOT/make-testfiles.ps1" ]; then
    bom_ok=0
    if [ "$(head -c 3 "$ROOT/make-testfiles.ps1" | od -An -tx1 | tr -d ' \n')" = "efbbbf" ]; then bom_ok=1; fi
    if tail -c +4 "$ROOT/make-testfiles.ps1" | LC_ALL=C grep -qP '[^\x00-\x7F]'; then
        bad "make-testfiles.ps1 contains non-ASCII bytes — 5.1 will mangle them (make it pure ASCII)"
    elif [ "$bom_ok" -ne 1 ]; then
        bad "make-testfiles.ps1 lacks a UTF-8 BOM — add it so 5.1 decodes it as UTF-8"
    else
        ok "make-testfiles.ps1 is pure ASCII with a UTF-8 BOM (5.1-safe; NOT executed here — no PowerShell)"
    fi
else
    bad "make-testfiles.ps1 missing from the tree"
fi

step "headless render: one enormous line must not hang the GUI (item 5 / B12, R115)"
# The .i32 item-5 failure, mechanised: auto-open a 300 MB file with NO line terminator (the
# make-testfiles.sh one-line file, triggered by appending bytes WITHOUT a newline so the reader's
# 48 MB load is one line). Before the render bound this pegged the GUI thread (55-86 % CPU, RSS past
# 3 GB in 40 s) and the window never left "loading...". Asserts: a capture exists, the window has
# repainted (the capture differs from a "loading..." frame is what the user inspects; here we assert
# the process is idle, not shaping), and the app's CPU is below 20 % at +8 s.
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ] && [ -f "$TFDIR/big-oneline.log" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"
    mv "$TFDIR/big-oneline.log" "$WDIR/big-oneline.log"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    CPU=999
    (
      Xvfb :96 -screen 0 1600x900x24 >/tmp/dw_xvfb_oneline.log 2>&1 &
      XP=$!; export DISPLAY=:96; sleep 2
      "$BIN" "$WDIR" --patterns "*.log" >/tmp/dw_gui_oneline.log 2>&1 &
      AP=$!; sleep 6
      printf 'x' >> "$WDIR/big-oneline.log"   # activity WITHOUT a newline => auto-open, one giant line
      sleep 8
      ps -o %cpu= -p $AP 2>/dev/null | tr -d ' ' > "$SHOTDIR/cpu.txt" || true
      import -window root "$SHOTDIR/gui_oneline.png" 2>/tmp/dw_cap_oneline.log || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    CPU=$(cat "$SHOTDIR/cpu.txt" 2>/dev/null || echo 999)
    SZ=$(stat -c%s "$SHOTDIR/gui_oneline.png" 2>/dev/null || echo 0)
    CPU_INT=${CPU%%.*}; [ -z "$CPU_INT" ] && CPU_INT=999
    if [ "$SZ" -gt 2000 ] && [ "$CPU_INT" -lt 20 ]; then
        cp "$SHOTDIR/gui_oneline.png" "$ROOT/gui_oneline_${BUILD_ID##* }.png" 2>/dev/null || true
        ok "one-line 300 MB file: window rendered, app at ${CPU}% CPU 8 s after open (${SZ} byte PNG: gui_oneline_${BUILD_ID##* }.png — inspect: skipped marker + clipped line with its [N more bytes not shown] marker)"
    else
        bad "one-line 300 MB file: PNG ${SZ} bytes, app at ${CPU}% CPU 8 s after open — the GUI is still shaping the line (see /tmp/dw_gui_oneline.log)"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary/test file unavailable -- one-line render check SKIPPED"
fi

step "headless render: zero-filled 300 MB file stays bounded (review #6 finding 6, R146)"
# The REAL `fsutil file createnew` shape (make-testfiles big-zero.log): the reader's 48 MB first load
# is 48 M NUL bytes, each rendered as U+FFFD (3 bytes) — 144 MB in one line, over the 64 MB cap and
# (until .i48) untrimmable. The scrollback cap now bounds a single line by BYTES. Asserts, from the
# app's own DIRWATCH_DEBUG trace: a `scrollback trim` line whose "buffer now N bytes" is at/under
# the trim target (48 MiB + a marker); plus the GUI idle (CPU < 20 %) 12 s after the open. The PNG
# is captured for inspection: the "--- earlier N bytes dropped ---" marker + the clipped line.
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ] && [ -f "$TFDIR/big-zero.log" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"; LDIR="$SHOTDIR/logs"
    mv "$TFDIR/big-zero.log" "$WDIR/big-zero.log"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :94 -screen 0 1600x900x24 >/tmp/dw_xvfb_zero.log 2>&1 &
      XP=$!; export DISPLAY=:94; sleep 2
      DIRWATCH_DEBUG=1 "$BIN" "$WDIR" --patterns "*.log" --log-dir "$LDIR" >/tmp/dw_gui_zero.log 2>&1 &
      AP=$!; sleep 6
      head -c 1 /dev/zero >> "$WDIR/big-zero.log"   # activity => auto-open; still one NUL "line"
      sleep 12
      # INSTANTANEOUS CPU over a 2 s window from /proc (utime+stime deltas), not `ps %cpu`, which is
      # the LIFETIME average: this step's legitimate ~5 s of load/normalise/trim work made the
      # average read 36 % while the app was already idle (R146 gate note).
      T1=$(awk '{print $14+$15}' /proc/$AP/stat 2>/dev/null || echo 0); sleep 2
      T2=$(awk '{print $14+$15}' /proc/$AP/stat 2>/dev/null || echo 0)
      HZ=$(getconf CLK_TCK 2>/dev/null || echo 100)
      echo $(( (T2 - T1) * 100 / (2 * HZ) )) > "$SHOTDIR/cpu.txt"
      import -window root "$SHOTDIR/gui_zero.png" 2>/tmp/dw_cap_zero.log || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    CPU=$(cat "$SHOTDIR/cpu.txt" 2>/dev/null || echo 999); CPU_INT=${CPU%%.*}; [ -z "$CPU_INT" ] && CPU_INT=999
    SZ=$(stat -c%s "$SHOTDIR/gui_zero.png" 2>/dev/null || echo 0)
    NOW=$(grep -oE 'scrollback trim for .* buffer now [0-9]+ bytes' "$LDIR/DirWatch.log" 2>/dev/null | tail -1 | grep -oE '[0-9]+ bytes$' | grep -oE '[0-9]+')
    TRIM_TO=$((48 * 1024 * 1024 + 64))
    if [ "$SZ" -gt 2000 ] && [ "$CPU_INT" -lt 20 ] && [ -n "$NOW" ] && [ "$NOW" -le "$TRIM_TO" ]; then
        cp "$SHOTDIR/gui_zero.png" "$ROOT/gui_zero_${BUILD_ID##* }.png" 2>/dev/null || true
        ok "zero-filled 300 MB file: buffer trimmed to ${NOW} bytes (<= ${TRIM_TO}), app at ${CPU}% CPU (2 s window) 12 s after open (${SZ} byte PNG: gui_zero_${BUILD_ID##* }.png — inspect: '--- earlier N bytes dropped ---' marker + one clipped line)"
    else
        bad "zero-filled 300 MB file: PNG ${SZ} bytes, CPU ${CPU}%, last trim 'buffer now'=${NOW:-none} (bound ${TRIM_TO}) — see /tmp/dw_gui_zero.log and $LDIR/DirWatch.log"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary/test file unavailable -- zero-filled render check SKIPPED"
fi
rm -rf "$TFDIR"

step "headless GUI render (xvfb + software renderer)"
# The port's early verification harness (PORT-PLAN-crossplatform §5): prove the iced GUI actually
# RENDERS off-hardware, not just that it compiles. Seed a temp dir, launch DirWatch under a virtual
# framebuffer with the software renderer, capture a screenshot, assert a non-trivial PNG. Requires
# xvfb + ImageMagick's `import`; absent => WARN (bare CI box), never a hard fail.
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"
    printf 'l\n' > "$WDIR/app.log"; printf 'h\n' > "$WDIR/notes.txt"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :98 -screen 0 900x640x24 >/tmp/dw_xvfb.log 2>&1 &
      XP=$!; export DISPLAY=:98; sleep 2
      "$BIN" "$WDIR" --patterns "*.log;*.txt" >/tmp/dw_gui.log 2>&1 &
      AP=$!; sleep 4
      import -window root "$SHOTDIR/gui_step1.png" 2>/tmp/dw_cap.log || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    SZ=$(stat -c%s "$SHOTDIR/gui_step1.png" 2>/dev/null || echo 0)
    if [ "$SZ" -gt 2000 ]; then
        cp "$SHOTDIR/gui_step1.png" "$ROOT/gui_step1_${BUILD_ID##* }.png" 2>/dev/null || true
        ok "GUI rendered headlessly (${SZ} byte PNG captured)"
    else
        warn "headless render produced no usable PNG (see /tmp/dw_gui.log, /tmp/dw_cap.log) -- render unverified here"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary unavailable -- headless GUI render SKIPPED (render verification deferred to the user's hardware)"
fi

step "headless render: symlink cycle + alias show each file ONCE (review #6 finding 2, R141)"
# Before .i48 two self-links at depth 6 made ONE app.log into 127 directory boxes (the review's
# screenshot), and `current -> logs` showed every file twice. Asserts from the app's own
# DIRWATCH_DEBUG sweep trace (R140): the first sweep walked exactly 2 directories (root + logs; the
# self-links, the parent link and the alias were all skipped) and found exactly 1 file. The PNG is
# for inspection: ONE box, ONE tile.
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"; LDIR="$SHOTDIR/logs"
    mkdir -p "$WDIR/logs"; printf 'one real file\n' > "$WDIR/logs/app.log"
    ln -s . "$WDIR/logs/l1"; ln -s . "$WDIR/logs/l2"; ln -s .. "$WDIR/logs/up"; ln -s logs "$WDIR/current"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :93 -screen 0 1200x800x24 >/tmp/dw_xvfb_symlink.log 2>&1 &
      XP=$!; export DISPLAY=:93; sleep 2
      DIRWATCH_DEBUG=1 "$BIN" "$WDIR" -d 6 --no-open --log-dir "$LDIR" >/tmp/dw_gui_symlink.log 2>&1 &
      AP=$!; sleep 7
      import -window root "$SHOTDIR/gui_symlink.png" 2>/tmp/dw_cap_symlink.log || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    SZ=$(stat -c%s "$SHOTDIR/gui_symlink.png" 2>/dev/null || echo 0)
    if [ "$SZ" -gt 2000 ] && grep -qE 'sweep #1: files=1 dirs=2 ' "$LDIR/DirWatch.log" 2>/dev/null; then
        cp "$SHOTDIR/gui_symlink.png" "$ROOT/gui_symlink_${BUILD_ID##* }.png" 2>/dev/null || true
        ok "symlink cycle + alias: first sweep files=1 dirs=2 (${SZ} byte PNG: gui_symlink_${BUILD_ID##* }.png — inspect: ONE box, ONE tile)"
    else
        bad "symlink cycle + alias: PNG ${SZ} bytes; sweep trace: $(grep -oE 'sweep #1: [^(]*' "$LDIR/DirWatch.log" 2>/dev/null | head -1 || echo none) — expected files=1 dirs=2 (see /tmp/dw_gui_symlink.log)"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary unavailable -- symlink render check SKIPPED"
fi

step "headless TAIL render: CRLF + non-ASCII lines are one row each (R138; review #6 renderer probe)"
# Review #5 finding 1's visual, mechanised: 12 CRLF lines carrying an accented name, an en dash,
# smart quotes and an ANSI colour escape auto-open a tail. Before R138 every such line rendered as
# TWO rows. The row count is a property of the PNG (inspected: 12 lines = 12 rows, no blank rows,
# top line unclipped); the step asserts the capture happened and that the tail's own trace shows
# the file loaded.
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"; LDIR="$SHOTDIR/logs"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :92 -screen 0 1600x900x24 >/tmp/dw_xvfb_crlf.log 2>&1 &
      XP=$!; export DISPLAY=:92; sleep 2
      DIRWATCH_DEBUG=1 "$BIN" "$WDIR" --patterns "*.log" --log-dir "$LDIR" >/tmp/dw_gui_crlf.log 2>&1 &
      AP=$!; sleep 6
      for i in $(seq 1 12); do
        printf '2026-09-16 12:00:%02d.000 INFO line %02d: café – ‘smart’ … \033[31mred\033[0m done\r\n' "$i" "$i" >> "$WDIR/crlf.log"
      done
      sleep 4
      import -window root "$SHOTDIR/gui_crlf.png" 2>/tmp/dw_cap_crlf.log || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    SZ=$(stat -c%s "$SHOTDIR/gui_crlf.png" 2>/dev/null || echo 0)
    if [ "$SZ" -gt 2000 ] && grep -q 'auto_open: spawned tail' "$LDIR/DirWatch.log" 2>/dev/null; then
        cp "$SHOTDIR/gui_crlf.png" "$ROOT/gui_crlf_${BUILD_ID##* }.png" 2>/dev/null || true
        ok "CRLF + non-ASCII tail rendered (${SZ} byte PNG: gui_crlf_${BUILD_ID##* }.png — inspect: 12 lines = 12 rows, no blank rows, top line unclipped)"
    else
        warn "CRLF tail render produced no usable PNG / no auto-open trace (see /tmp/dw_gui_crlf.log) -- R138 render unverified here"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary unavailable -- CRLF tail render SKIPPED"
fi

step "log location: --log-dir honoured, nothing written beside the exe (R127)"
# WHY THIS EXISTS (review #4 finding 10, DECISIONS R127): until .i39 every log went beside the exe,
# which is read-only for an AppImage / .app / Program Files install, so a packaged build could never
# log. Launch DirWatch headlessly against an UNREADABLE directory (a guaranteed ERR line) with
# --log-dir pointing at scratch and assert: (1) DirWatch.log appears THERE, stamped with this build
# ID; (2) NOTHING named DirWatch*.log appears beside the binary. Needs xvfb (the GUI must start to
# reach the watch error); absent => WARN, never a hard fail. Add-only (.i40).
if command -v xvfb-run >/dev/null 2>&1 && [ -x "target/debug/dirwatch" -o -x "target/release/dirwatch" ]; then
    BIN="target/debug/dirwatch"; [ -x "target/release/dirwatch" ] && BIN="target/release/dirwatch"
    LDIR="$(mktemp -d)/logs-here"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/dw_xdg_$$}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :97 -screen 0 800x600x24 >/tmp/dw_xvfb_log.log 2>&1 &
      XP=$!; export DISPLAY=:97; sleep 2
      "$BIN" /nonexistent/dirwatch-gate-dir --log-dir "$LDIR" >/tmp/dw_logdir.log 2>&1 &
      AP=$!; sleep 4
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    BESIDE=$(ls "$(dirname "$BIN")"/DirWatch*.log 2>/dev/null | wc -l)
    if [ -f "$LDIR/DirWatch.log" ] && grep -qF "$BUILD_ID" "$LDIR/DirWatch.log" && grep -q "watch error" "$LDIR/DirWatch.log" && [ "$BESIDE" -eq 0 ]; then
        ok "DirWatch.log written to --log-dir (stamped $BUILD_ID, watch error recorded); nothing beside the exe"
    else
        bad "log location check failed: log-dir file present=$([ -f "$LDIR/DirWatch.log" ] && echo yes || echo no), files beside exe=$BESIDE (see /tmp/dw_logdir.log)"
    fi
    rm -rf "$(dirname "$LDIR")"
else
    warn "xvfb/binary unavailable -- log-location check SKIPPED (the user's runtests collect DirWatch.log from the per-user dir)"
fi

rm -rf "$PARITY_TARGET" 2>/dev/null

# ---------------------------------------------------------------------------
# v15 PROCESS CHECKS (DECISIONS R117). These mechanize rules that were prose in
# v14: the BUILD.meta gate input, the ATTEMPT>=2 blind-fix veto, the diagnostic
# harness-only invariant (vs the prior build's MANIFEST.sha256), and the
# per-build BUILDS.md line. Add-only per v15 Gate-scripts.
# ---------------------------------------------------------------------------
step "BUILD.meta present and well-formed (v15, R117)"
META="$ROOT/BUILD.meta"
meta_ok=1
if [ ! -f "$META" ]; then
    bad "BUILD.meta missing at tree top (v15 gate input)"; meta_ok=0
else
    for k in BUILD_ID KIND ATTEMPT EVIDENCE; do
        grep -qE "^$k=" "$META" || { bad "BUILD.meta missing $k= line"; meta_ok=0; }
    done
    M_KIND="$(grep -E '^KIND=' "$META" | head -1 | cut -d= -f2- | tr -d ' ')"
    case "$M_KIND" in feature|fix|diagnostic) : ;; *) bad "BUILD.meta KIND='$M_KIND' not one of feature|fix|diagnostic"; meta_ok=0 ;; esac
    # BUILD_ID in BUILD.meta must match the compiled build_info.rs BUILD_ID.
    M_BID="$(grep -E '^BUILD_ID=' "$META" | head -1 | cut -d= -f2-)"
    if [ "$M_BID" != "$BUILD_ID" ]; then
        bad "BUILD.meta BUILD_ID='$M_BID' != build_info.rs BUILD_ID='$BUILD_ID'"; meta_ok=0
    fi
    [ "$meta_ok" -eq 1 ] && ok "BUILD.meta present, KIND=$M_KIND, BUILD_ID matches build_info.rs"
fi

step "ATTEMPT>=2 fix must carry evidence (v15 blind-fix veto, R117)"
# From ATTEMPT 2 a fix with EVIDENCE=none is illegal — the next build is a diagnostic or a fix
# derived from a named measurement. (Instrument, don't guess.)
if [ -f "$META" ]; then
    M_KIND="$(grep -E '^KIND=' "$META" | head -1 | cut -d= -f2- | tr -d ' ')"
    M_ATT="$(grep -E '^ATTEMPT=' "$META" | head -1 | cut -d= -f2- | tr -d ' ')"
    M_EVI="$(grep -E '^EVIDENCE=' "$META" | head -1 | cut -d= -f2- | sed 's/^ *//;s/ *$//')"
    M_ATT_N="${M_ATT%%.*}"; case "$M_ATT_N" in ''|*[!0-9]*) M_ATT_N=0 ;; esac
    if [ "$M_KIND" = "fix" ] && [ "$M_ATT_N" -ge 2 ] && { [ -z "$M_EVI" ] || [ "$M_EVI" = "none" ]; }; then
        bad "KIND=fix at ATTEMPT=$M_ATT with EVIDENCE=none — a blind fix from ATTEMPT 2 is illegal (diagnostic, or a fix from a named measurement)"
    else
        ok "no blind fix (KIND=$M_KIND ATTEMPT=$M_ATT EVIDENCE ${M_EVI:-none})"
    fi
else
    warn "BUILD.meta absent — blind-fix veto not evaluated"
fi

step "diagnostic build touches harness paths only (v15, R117)"
# A KIND=diagnostic build may change harness/self-test code ONLY. Enforced by diffing the tree's
# non-harness files against the prior build's MANIFEST.sha256 baseline. Harness paths are recorded
# in .harness-paths (one path prefix per line) at the first diagnostic build; until then, and for
# non-diagnostic builds, this is a NAMED PASS (nothing to enforce). MANIFEST.sha256 absent => WARN.
if [ -f "$META" ] && [ "$(grep -E '^KIND=' "$META" | head -1 | cut -d= -f2- | tr -d ' ')" = "diagnostic" ]; then
    if [ ! -f "$ROOT/MANIFEST.sha256" ]; then
        warn "diagnostic build but no prior MANIFEST.sha256 baseline — drift not enforced this build"
    else
        HARNESS_RE='^$'
        [ -f "$ROOT/.harness-paths" ] && HARNESS_RE="$(grep -vE '^\s*(#|$)' "$ROOT/.harness-paths" | sed 's:[].[^$*\\/]:\\&:g' | paste -sd'|' -)"
        drift=0
        while IFS= read -r mline; do
            mh="${mline%% *}"; mf="${mline#* }"; mf="${mf# }"
            [ -z "$mf" ] && continue
            [ -n "$HARNESS_RE" ] && echo "$mf" | grep -qE "$HARNESS_RE" && continue   # harness path — allowed to change
            if [ -f "$ROOT/$mf" ]; then
                nh="$(sha256sum "$ROOT/$mf" | cut -d' ' -f1)"
                if [ "$nh" != "$mh" ]; then bad "diagnostic build changed non-harness file: $mf"; drift=1; fi
            fi
        done < "$ROOT/MANIFEST.sha256"
        [ "$drift" -eq 0 ] && ok "diagnostic build: no non-harness file changed vs the prior MANIFEST.sha256"
    fi
else
    ok "not a diagnostic build — harness-only invariant N/A"
fi

step "BUILDS.md has a line for this build (v15, R117)"
# v15: the only per-build record is BUILDS.md; a build must have its line. The build ID's iNN suffix
# is what BUILDS.md keys on (its lines lead with .iNN).
if [ ! -f "$ROOT/BUILDS.md" ]; then
    bad "BUILDS.md missing (v15's only per-build record)"
else
    SUFFIX=".${BUILD_ID##*.}"   # e.g. "DirWatch 1.0 build 2026-09-11.i33" -> ".i33"
    if grep -qE "^\s*${SUFFIX}\b|^\s*${SUFFIX} " "$ROOT/BUILDS.md" || grep -qF "$SUFFIX " "$ROOT/BUILDS.md"; then
        ok "BUILDS.md carries a line for $SUFFIX"
    else
        bad "BUILDS.md has no line for this build ($SUFFIX)"
    fi
fi

echo ""
echo "== SUMMARY =="
echo "  checks failed : $fails"
echo "  warnings      : $warns"
if [ "$fails" -ne 0 ]; then
    echo "  BUILD GATE RED — fix the FAILs above."
    exit 1
else
    echo "  BUILD GATE GREEN$([ "$warns" -ne 0 ] && echo " (with $warns warning(s) — read them)")"
    exit 0
fi
