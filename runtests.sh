#!/usr/bin/env bash
# runtests.sh - the DirWatch iced-port test harness for LINUX (x86_64 / aarch64).
#
# Run it from the project root (the top of test/ after extracting the build zip):
#
#     bash runtests.sh
#
# WHAT IT DOES, in one run (the Linux analogue of runtests.command / runtests.ps1):
#   1. Toolchain preflight: cargo present; the host target is the native one (no cross-lipo on
#      Linux - a single-arch native binary is the Linux ship shape, unlike the Mac universal build).
#   2. cargo fmt --check, clippy -D warnings, build --release, test (full suite), one PASS/FAIL line
#      each; any failure makes the whole run FAIL (nonzero exit).
#   3. Copies the release binary into dist/ and confirms it launches (`dist/dirwatch --version`
#      exits 0 - the key CRT-static "runs with no missing lib" check on Windows; --selftest was
#      removed at .i31, DECISIONS R106). The headless render (step 4) is the real runtime proof.
#   4. HEADLESS GUI RENDER: launches DirWatch under a virtual framebuffer (xvfb) with the software
#      renderer over a seeded temp dir and captures a screenshot into dist/. This is the Linux
#      equivalent of the Mac screenshot / Windows PrintWindow step - it proves the iced GUI actually
#      RENDERS on Linux, not merely that it compiles (PORT-PLAN-crossplatform §5). Requires xvfb +
#      ImageMagick's `import`; if either is absent the render is a NAMED WARN (not a FAIL) so the
#      harness still completes on a bare box - but a real sign-off run should have produced the shot.
#   5. Standalone check: exe size + no sidecar .so beside it (single self-contained binary - the
#      port's hard requirement).
#   6. Zips ONE artifacts_<serial>.zip at the top of the project (the run log, the render's
#      this build's lines from DirWatch.log - the per-user log at ${XDG_STATE_HOME:-~/.local/state}/dirwatch
#      since .i40 (DECISIONS R127), filtered by build ID - a crash log if the app panicked, and the render
#      screenshot). That single file is what
#      you upload.
#
# checks.sh is NOT this - it is the assistant's off-hardware gate. This runtests.sh is the user's Linux
# gate, the same role runtests.ps1 plays on Windows and runtests.command on macOS. checks.sh runs
# this script in a scratch copy for gate parity (v14: "if the user runs it, the gate ran it").

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
cd "$ROOT"

BUILD_ID="$(grep -oE 'BUILD_ID[^"]*"[^"]+"' crates/dirwatch-core/src/build_info.rs 2>/dev/null | grep -oE '"[^"]+"' | tr -d '"' | head -1)"
SERIAL="${BUILD_ID##* }"; [ -n "$SERIAL" ] || SERIAL="unknown"
LOG="$ROOT/runtests_output_$SERIAL.txt"
ART="$ROOT/artifacts_$SERIAL.zip"
DIST="$ROOT/dist"

fails=0
# Log to BOTH the console and a UTF-8 file (no redirect hides output).
: > "$LOG"
log()  { printf '%s\n' "$*" | tee -a "$LOG"; }
pass() { log "  [PASS] $1"; }
bad()  { log "  [FAIL] $1"; fails=$((fails+1)); }
warn() { log "  [WARN] $1"; }
step() { log ""; log "== $1 =="; }
# Run a command, streaming its output to console + log, returning its exit code.
run()  { "$@" 2>&1 | tee -a "$LOG"; return "${PIPESTATUS[0]}"; }

log "=== DirWatch runtests (Linux) - ${BUILD_ID:-<unknown build id>} ==="
log "host:  $(uname -srm)"
log "date:  $(date)"
log "rustc: $(rustc --version 2>/dev/null)"
log "cargo: $(cargo --version 2>/dev/null)"
log "bash:  $(bash --version 2>/dev/null | head -1)"

# --- 1. toolchain preflight ----------------------------------------------------------------------
step "toolchain preflight"
if command -v cargo >/dev/null 2>&1; then
    pass "cargo present"
else
    bad "cargo not found - install Rust (https://rustup.rs) and re-run"
fi

# --- 2. fmt / clippy -----------------------------------------------------------------------------
step "cargo fmt --check"
if run cargo fmt --all --check; then pass "formatting clean"; else bad "cargo fmt reported diffs"; fi

step "cargo clippy (deny warnings)"
if run cargo clippy --locked --workspace --all-targets -- -D warnings; then pass "clippy clean (no warnings)"; else bad "clippy warnings/errors"; fi

# --- 3. release build ----------------------------------------------------------------------------
BIN_HOST=""
step "cargo build --release (native Linux)"
if run cargo build --locked --release --workspace; then
    BIN_HOST="target/release/dirwatch"; pass "native release built"
else
    bad "native release build failed"
fi

# --- 4. full test suite --------------------------------------------------------------------------
step "cargo test --workspace (full suite)"
if run cargo test --locked --workspace; then pass "test suite green"; else bad "test suite failed"; fi

# --- dependency audit (review #2 finding 12, DECISIONS R113) -------------------------------------
# `cargo audit` checks the SHIPPED lockfile (deterministic since R98) against the RustSec database.
# FAIL on a vulnerability; informational (unmaintained) advisories are printed, not failed. The one
# ignore is recorded: RUSTSEC-2026-0253 (`lru` pop() panic-safety) needs an UNWINDING panic and the
# release profile is `panic = "abort"`, so it cannot manifest in the shipped binary; re-judge it
# whenever Cargo.lock changes. Missing tool => NAMED WARN with the install line (one-time setup);
# no network (advisory DB fetch fails) => NAMED WARN. Both are stated, never silent.
step "cargo audit (RustSec, shipped Cargo.lock)"
if cargo audit --version >/dev/null 2>&1; then
    if run cargo audit --ignore RUSTSEC-2026-0253; then
        pass "cargo audit: no known vulnerabilities in Cargo.lock (RUSTSEC-2026-0253 ignored: panic=abort, DECISIONS R113)"
    else
        rc=$?
        # cargo-audit exits 1 for vulnerabilities AND for a DB fetch failure; tell them apart by the
        # log so an offline box gets a WARN, not a false FAIL.
        if tail -30 "$LOG" | grep -qiE "couldn.t fetch|failed to fetch|error fetching|Fetching advisory|could not connect|network|resolve host"; then
            warn "cargo audit could not fetch the advisory database (offline?) - audit SKIPPED, run it online"
        else
            bad "cargo audit found a vulnerability in Cargo.lock (exit $rc) - read the advisory above"
        fi
    fi
else
    warn "cargo-audit not installed - audit SKIPPED. One-time setup: cargo install cargo-audit --locked"
fi

# --- 5. package binary into dist/ ----------------------------------------------------------------
# (--selftest removed at .i31/release-prep, DECISIONS R106: no dev self-test flag in the shipped
# binary. Runtime proof is now the headless GUI render below, which launches the REAL binary; the
# `--version` line just confirms the exe launches + links — the key CRT-static check on Windows,
# harmless here.)
step "package binary into dist/"
mkdir -p "$DIST"
if [ -x "$BIN_HOST" ]; then
    cp "$BIN_HOST" "$DIST/dirwatch"
    pass "native dirwatch copied to dist/"
else
    bad "native release binary missing - cannot package"
fi

if [ -x "$DIST/dirwatch" ]; then
    step "binary launches (--version exits 0)"
    if ( cd "$DIST" && ./dirwatch --version ) 2>&1 | tee -a "$LOG"; then
        [ "${PIPESTATUS[0]}" -eq 0 ] && pass "dirwatch --version exited 0" || bad "dirwatch --version failed"
    else bad "dirwatch --version failed"; fi
fi

# --- 6. headless GUI render (xvfb + software renderer) --------------------------------------------
# The Linux equivalent of the Mac screenshot / Windows PrintWindow: prove the iced GUI RENDERS on
# Linux, not merely that it compiles (PORT-PLAN-crossplatform §5). Absent xvfb/import => NAMED WARN.
step "headless GUI render (xvfb + software renderer)"
if command -v xvfb-run >/dev/null 2>&1 && command -v import >/dev/null 2>&1 && [ -x "$DIST/dirwatch" ]; then
    SHOTDIR="$(mktemp -d)"; WDIR="$(mktemp -d)"
    printf 'sample log line\n' > "$WDIR/app.log"; printf 'sample text\n' > "$WDIR/notes.txt"
    export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$SHOTDIR/xdg}"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :95 -screen 0 900x640x24 >"$SHOTDIR/xvfb.log" 2>&1 &
      XP=$!; export DISPLAY=:95; sleep 2
      # --no-open so the render captures the main window grid deterministically (no tail spawn).
      # DIRWATCH_DEBUG=1 so the run writes its trace to DirWatch.log (per-user log dir, R127) for the
      # artifact zip (replaces the runtime log the removed --selftest used to produce; B6/.i31, R103/R106).
      DIRWATCH_DEBUG=1 "$DIST/dirwatch" "$WDIR" --patterns "*.log;*.txt" --no-open >"$SHOTDIR/gui.log" 2>&1 &
      AP=$!; sleep 4
      import -window root "$DIST/gui_linux_$SERIAL.png" 2>"$SHOTDIR/cap.log" || true
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    SZ=$(stat -c%s "$DIST/gui_linux_$SERIAL.png" 2>/dev/null || echo 0)
    if [ "$SZ" -gt 2000 ]; then
        pass "GUI rendered headlessly ($SZ byte PNG: dist/gui_linux_$SERIAL.png)"
    else
        warn "headless render produced no usable PNG (see the run log) - render unverified this run"
    fi
    rm -rf "$SHOTDIR" "$WDIR"
else
    warn "xvfb/import/binary unavailable - headless GUI render SKIPPED (install xvfb + imagemagick to enable)"
fi

# --- 7. standalone check (size + no sidecar .so) -------------------------------------------------
step "standalone exe check (size + no sidecar .so)"
if [ -x "$DIST/dirwatch" ]; then
    SZ=$(stat -c%s "$DIST/dirwatch" 2>/dev/null || echo 0)
    log "  exe size: $SZ bytes (~$(awk "BEGIN{printf \"%.2f\", $SZ/1048576}") MB)"
    if ls "$DIST"/*.so >/dev/null 2>&1; then
        bad "sidecar .so present beside the exe"
    else
        pass "no sidecar .so beside the exe (single self-contained binary)"
    fi
else
    bad "no dirwatch binary in dist/ - cannot size-check"
fi

# --- 7b. search-scan bench on hardware (.i54, R161) ----------------------------------------------
# The search-per-keystroke MEASUREMENT. Run the exe with BOTH gate env vars set; it times the real
# refresh_matches scan across buffer sizes up to the 48 MB load cap / 64 MB scrollback ceiling, writes
# BENCH lines to the per-user DirWatch.log (NO --log-dir, so § 8 collects them by the same build-ID
# filter), and exits without a window. Env-gated + debug-only. Deferred platform (R160) but kept in
# sync with runtests.ps1 (gate parity).
step "search-scan bench (real per-keystroke scan cost)"
if [ -x "$DIST/dirwatch" ]; then
    DIRWATCH_DEBUG=1 DIRWATCH_SEARCH_BENCH=1 "$DIST/dirwatch" >/tmp/dw_bench_run.log 2>&1
    LDIR="${XDG_STATE_HOME:-}"; case "$LDIR" in /*) LDIR="$LDIR/dirwatch" ;; *) LDIR="$HOME/.local/state/dirwatch" ;; esac
    BLOG="$LDIR/DirWatch.log"
    if [ ! -f "$BLOG" ]; then
        bad "search-bench: no DirWatch.log at $LDIR"
    elif ! grep -F -- "$BUILD_ID" "$BLOG" | grep -q "search-scan sweep END"; then
        bad "search-bench: sweep did not complete for $BUILD_ID (see /tmp/dw_bench_run.log)"
    elif ! grep -F -- "$BUILD_ID" "$BLOG" | grep -qE 'buffer=48MB\(INITIAL_LOAD_CAP\).*query="error".*scan=[0-9.]+ms'; then
        bad "search-bench: no 48 MB (load-cap) measurement"
    elif ! grep -F -- "$BUILD_ID" "$BLOG" | grep -qE 'buffer=64MB\(SCROLLBACK_CAP\).*query="error".*scan=[0-9.]+ms'; then
        bad "search-bench: no 64 MB (scrollback-cap) measurement"
    else
        WORST="$(grep -F -- "$BUILD_ID" "$BLOG" | grep -oE 'buffer=64MB\(SCROLLBACK_CAP\).*query="error" matches=[0-9+]+ scan=[0-9.]+ms' | tail -1)"
        pass "search-scan bench ran + stamped; worst case: $WORST"
    fi
else
    bad "no dirwatch binary in dist/ - cannot run search bench"
fi

# --- 7c. Linux SHIP SHAPES + packaging assertions (1.0 packaging, R168) --------------------------
# Build the three Linux ship shapes (AppImage + .deb + .rpm) into dist/ and run the packaging
# assertions: the ldd "self-contained, base set only" runtime-dependency assertion (the Linux
# analogue of the Windows dumpbin/dependents + macOS otool -L assertions), and the per-shape log-dir
# verify (R127: the AppImage, running from a read-only mount, still logs to the per-user dir). The
# shape-building + assertion logic lives in ONE sourced helper (package-linux.sh) shared with the
# off-hardware gate checks.sh, so it is not duplicated (v15 "share scaffolding between sibling
# tools"). Missing packaging tool => that shape is a NAMED WARN inside the helper; a built shape with
# a broken payload, or a failed ldd/log-dir assertion, is a FAIL that fails the whole run.
step "Linux ship shapes (AppImage + deb + rpm) + packaging assertions (R168)"
if [ -f "$ROOT/package-linux.sh" ]; then
    # Give the helper the harness's own PASS/FAIL/log functions so its lines fold into this run and a
    # packaging failure increments the harness's fail count (one RESULT for the whole gate).
    pkg_log()  { log "$*"; }
    pkg_pass() { pass "$1"; }
    pkg_bad()  { bad "$1"; }
    pkg_warn() { warn "$1"; }
    export DIST
    # shellcheck source=package-linux.sh
    . "$ROOT/package-linux.sh"
    pkg_run_all || true          # its failures already went through bad() -> the harness fail count
else
    warn "package-linux.sh not present - Linux ship shapes NOT built this run (1.0 packaging helper missing)"
fi

# --- 8. collect ONE artifact zip -----------------------------------------------------------------
step "collecting ONE artifact zip: artifacts_$SERIAL.zip"
items=("$LOG")
[ -f "$DIST/dirwatch" ] && items+=("$DIST/dirwatch")
# The built ship shapes ride in the artifact zip so the user has the actual AppImage/.deb/.rpm to install.
for shape in "$DIST"/dirwatch-*.AppImage "$DIST"/dirwatch_*.deb "$DIST"/dirwatch-*.rpm; do
    [ -f "$shape" ] && items+=("$shape")
done
# DirWatch.log lives in the per-user log directory since .i40 (DECISIONS R127) and persists across
# builds, so collect ONLY this build's lines (live file + its one rotated .1) into DirWatch_<serial>.log;
# the crash log is copied whole. Same rule applog.rs applies (Os::Other): $XDG_STATE_HOME if absolute,
# else ~/.local/state.
LOGDIR="${XDG_STATE_HOME:-}"
case "$LOGDIR" in /*) LOGDIR="$LOGDIR/dirwatch" ;; *) LOGDIR="$HOME/.local/state/dirwatch" ;; esac
DWLOG="$DIST/DirWatch_$SERIAL.log"; rm -f "$DWLOG"
for f in "$LOGDIR/DirWatch.log.1" "$LOGDIR/DirWatch.log"; do
    [ -f "$f" ] && grep -F -- "$BUILD_ID" "$f" >> "$DWLOG" 2>/dev/null
done
if [ -s "$DWLOG" ]; then
    log "  DirWatch.log: $(wc -l < "$DWLOG") line(s) stamped $BUILD_ID collected from $LOGDIR"
    items+=("$DWLOG")
else
    log "  DirWatch.log: no lines stamped $BUILD_ID in $LOGDIR (nothing logged this build)"
    rm -f "$DWLOG"
fi
[ -f "$LOGDIR/DirWatch_crash.log" ] && items+=("$LOGDIR/DirWatch_crash.log")
for s in "$DIST"/gui_linux_*.png; do [ -f "$s" ] && items+=("$s"); done
rm -f "$ART"
if zip -j -q "$ART" "${items[@]}"; then
    log "wrote $(basename "$ART") containing:"
    for i in "${items[@]}"; do log "  $(basename "$i")"; done
else
    bad "failed to write artifact zip"
fi

log ""
if [ "$fails" -gt 0 ]; then
    log "== RESULT: FAIL ($fails step(s)) - read the FAILs above =="
    exit 1
else
    log "== RESULT: PASS - upload $(basename "$ART") =="
    exit 0
fi
