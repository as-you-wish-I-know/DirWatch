#!/usr/bin/env bash
# runtests.command - the DirWatch iced-port test harness for macOS (Intel + Apple Silicon).
#
# Run it from the project root (the top of test/ after extracting the build zip). Because it ends
# in .command it is double-clickable in Finder; from a terminal:
#
#     bash runtests.command
#
# WHAT IT DOES, in one run (the macOS analogue of runtests.ps1):
#   1. Ensures the two Apple rustup targets (aarch64-apple-darwin + x86_64-apple-darwin).
#   2. cargo fmt --check, clippy -D warnings, build --release for BOTH targets, test (full suite).
#      One PASS/FAIL line each; any failure makes the whole run FAIL.
#   3. lipo's the two release binaries into ONE UNIVERSAL dirwatch under dist/ (runs on Intel AND
#      Apple Silicon), and confirms it launches (`dist/dirwatch --version` exits 0 - which also
#      proves the universal binary runs on this arch; --selftest was removed at .i31, DECISIONS R106).
#   4. Reports the universal exe size and confirms no sidecar .dylib beside it (single self-contained
#      binary - the port's hard requirement).
#   5. Zips ONE artifacts_<serial>.zip at the top of the project (the run log, a crash log if the app
#      panicked, and any screenshot you dropped in dist/). That single file is what you upload.
#
# checks.sh is NOT this - it is the assistant's off-hardware gate. This .command is the user's Mac gate.
#
# PORTABLE-STEP FALLBACK (gate parity): on a NON-macOS host - e.g. the assistant's Linux checks.sh
# running this script to satisfy "if the user runs it, the gate ran it" - the Mac-only universal-lipo
# step is SKIPPED and NAMED (a host-arch binary is used instead so --version + the zip still run).
# The universal build itself is verifiable only on macOS.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
cd "$ROOT"

OS="$(uname -s)"
IS_MAC=0; [ "$OS" = "Darwin" ] && IS_MAC=1

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

log "=== DirWatch runtests (macOS) - ${BUILD_ID:-<unknown build id>} ==="
log "host: $(uname -srm)"
log "date: $(date)"
log "rustc: $(rustc --version 2>/dev/null)"
log "cargo: $(cargo --version 2>/dev/null)"
[ "$IS_MAC" -eq 1 ] || log "NOTE: non-macOS host - Mac-only universal build will be SKIPPED (portable steps still run)."

# --- 1. toolchain / targets ----------------------------------------------------------------------
step "toolchain preflight"
if ! command -v cargo >/dev/null 2>&1; then
    bad "cargo not found - install Rust (https://rustup.rs) and re-run"
fi
if [ "$IS_MAC" -eq 1 ]; then
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        if rustup target list --installed 2>/dev/null | grep -q "^$t$"; then
            pass "$t installed"
        elif rustup target add "$t" >>"$LOG" 2>&1; then
            pass "$t installed (added now)"
        else
            bad "could not add rustup target $t"
        fi
    done
fi

# --- 2. fmt / clippy -----------------------------------------------------------------------------
step "cargo fmt --check"
if run cargo fmt --all --check; then pass "formatting clean"; else bad "cargo fmt reported diffs"; fi

step "cargo clippy (deny warnings)"
if run cargo clippy --locked --workspace --all-targets -- -D warnings; then pass "clippy clean (no warnings)"; else bad "clippy warnings/errors"; fi

# --- 3. release build ----------------------------------------------------------------------------
BIN_ARM=""; BIN_X86=""; BIN_HOST=""
if [ "$IS_MAC" -eq 1 ]; then
    step "cargo build --release (aarch64-apple-darwin)"
    if run cargo build --locked --release --workspace --target aarch64-apple-darwin; then
        BIN_ARM="target/aarch64-apple-darwin/release/dirwatch"; pass "arm64 release built"
    else bad "arm64 release build failed"; fi

    step "cargo build --release (x86_64-apple-darwin)"
    if run cargo build --locked --release --workspace --target x86_64-apple-darwin; then
        BIN_X86="target/x86_64-apple-darwin/release/dirwatch"; pass "x86_64 release built"
    else bad "x86_64 release build failed"; fi
else
    step "cargo build --release (host - non-mac fallback)"
    if run cargo build --locked --release --workspace; then
        BIN_HOST="target/release/dirwatch"; pass "host release built"
    else bad "host release build failed"; fi
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

# --- 5. universal binary (mac) or host binary (fallback) -----------------------------------------
# (--selftest removed at .i31/release-prep, DECISIONS R106. Runtime proof is `--version` exits 0,
# which also confirms the universal binary actually launches on this arch.)
step "package binary into dist/"
mkdir -p "$DIST"
if [ "$IS_MAC" -eq 1 ]; then
    if [ -x "$BIN_ARM" ] && [ -x "$BIN_X86" ]; then
        if lipo -create -output "$DIST/dirwatch" "$BIN_ARM" "$BIN_X86" 2>>"$LOG"; then
            pass "universal dirwatch (arm64 + x86_64)"
            log "    $(lipo -info "$DIST/dirwatch" 2>/dev/null)"
        else bad "lipo failed to create the universal binary"; fi
    else
        bad "one or both apple release binaries missing - cannot lipo"
    fi
else
    if [ -x "$BIN_HOST" ]; then
        cp "$BIN_HOST" "$DIST/dirwatch"
        warn "non-macOS: dist/dirwatch is the HOST binary, NOT a universal Mac binary (lipo SKIPPED)"
    else
        bad "host release binary missing - cannot package"
    fi
fi

if [ -x "$DIST/dirwatch" ]; then
    step "binary launches (--version exits 0)"
    if ( cd "$DIST" && ./dirwatch --version ) 2>&1 | tee -a "$LOG"; then
        [ "${PIPESTATUS[0]}" -eq 0 ] && pass "dirwatch --version exited 0" || bad "dirwatch --version failed"
    else bad "dirwatch --version failed"; fi
fi

# --- 6. standalone check (size + no sidecar dylibs) ----------------------------------------------
step "standalone exe check (size + no sidecar .dylib)"
if [ -x "$DIST/dirwatch" ]; then
    SZ=$(stat -f%z "$DIST/dirwatch" 2>/dev/null || stat -c%s "$DIST/dirwatch" 2>/dev/null || echo 0)
    log "  exe size: $SZ bytes (~$(awk "BEGIN{printf \"%.2f\", $SZ/1048576}") MB)"
    if ls "$DIST"/*.dylib >/dev/null 2>&1; then
        bad "sidecar .dylib present beside the exe"
    else
        pass "no sidecar .dylib beside the exe (single self-contained binary)"
    fi
else
    bad "no dirwatch binary in dist/ - cannot size-check"
fi

# --- 6c. macOS .app bundle + otool -L assertion + .app.zip / -src.zip (1.0 packaging, R168/R171) --
# Assemble a UNIVERSAL DirWatch.app (unsigned; first launch is right-click -> Open), assert it depends
# ONLY on system libraries (otool -L: /usr/lib + /System/Library — the macOS analogue of the Linux ldd
# base-set and the Windows dumpbin/dependents assertions), and produce DirWatch-<serial>.app.zip plus a
# buildable DirWatch-<serial>-src.zip. Both ride in the artifact zip (§7). On a NON-macOS host (checks.sh
# gate parity) the bundle is assembled from the host binary so the layout + zip logic is exercised, and
# iconutil / otool / the universal lipo are NAMED mac-only skips (the real universal .app is verifiable
# only on macOS — same honesty as the universal-lipo fallback above).
step "macOS .app bundle + otool -L (self-contained: system libs only)"
APP="$DIST/DirWatch.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
if [ -x "$DIST/dirwatch" ]; then
    cp "$DIST/dirwatch" "$APP/Contents/MacOS/dirwatch"
    VER="$(grep -m1 -E '^version = ' crates/dirwatch/Cargo.toml | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')"
    cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DirWatch</string>
  <key>CFBundleDisplayName</key><string>DirWatch</string>
  <key>CFBundleIdentifier</key><string>net.mikea.dirwatch</string>
  <key>CFBundleVersion</key><string>${SERIAL}</string>
  <key>CFBundleShortVersionString</key><string>${VER:-1.0.0}</string>
  <key>CFBundleExecutable</key><string>dirwatch</string>
  <key>CFBundleIconFile</key><string>dirwatch</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
    printf 'APPL????' > "$APP/Contents/PkgInfo"
    if [ "$IS_MAC" -eq 1 ]; then
        # Icon: build an .iconset from the shipped PNGs and iconutil it to .icns (iconutil ships with
        # the Xcode command-line tools). Non-fatal if a size is missing — the app runs regardless.
        ICONSET="$DIST/dirwatch.iconset"; rm -rf "$ICONSET"; mkdir -p "$ICONSET"
        cp packaging/linux/icons/dirwatch-16.png  "$ICONSET/icon_16x16.png"      2>/dev/null || true
        cp packaging/linux/icons/dirwatch-32.png  "$ICONSET/icon_16x16@2x.png"   2>/dev/null || true
        cp packaging/linux/icons/dirwatch-32.png  "$ICONSET/icon_32x32.png"      2>/dev/null || true
        cp packaging/linux/icons/dirwatch-64.png  "$ICONSET/icon_32x32@2x.png"   2>/dev/null || true
        cp packaging/linux/icons/dirwatch-128.png "$ICONSET/icon_128x128.png"    2>/dev/null || true
        cp packaging/linux/icons/dirwatch-256.png "$ICONSET/icon_128x128@2x.png" 2>/dev/null || true
        cp packaging/linux/icons/dirwatch-256.png "$ICONSET/icon_256x256.png"    2>/dev/null || true
        cp packaging/linux/icons/dirwatch-512.png "$ICONSET/icon_256x256@2x.png" 2>/dev/null || true
        cp packaging/linux/icons/dirwatch-512.png "$ICONSET/icon_512x512.png"    2>/dev/null || true
        if command -v iconutil >/dev/null 2>&1 && iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/dirwatch.icns" 2>>"$LOG"; then
            log "    built dirwatch.icns from the shipped icon set"
        else
            warn "iconutil could not build dirwatch.icns (the .app still runs; the icon is cosmetic)"
        fi
        rm -rf "$ICONSET"
        # otool -L on the universal binary: ONLY /usr/lib + /System/Library are allowed. Any other
        # .dylib or non-system .framework is a bundled/sidecar runtime dependency and FAILS.
        otool_out="$(otool -L "$APP/Contents/MacOS/dirwatch" 2>&1)"
        printf '%s\n' "$otool_out" | sed 's/^/    /' >> "$LOG"
        printf '%s\n' "$otool_out" | sed 's/^/    /'
        offenders="$(printf '%s\n' "$otool_out" | tail -n +2 | awk '{print $1}' \
            | grep -vE '^/usr/lib/|^/System/Library/' | grep -E '\.dylib|\.framework' || true)"
        if [ -n "$offenders" ]; then
            bad "otool -L shows NON-system dependencies (a self-contained .app must not): $(printf '%s' "$offenders" | tr '\n' ' ')"
        else
            pass "otool -L: only /usr/lib + /System/Library (no bundled/sidecar .dylib or non-system framework)"
        fi
    else
        warn "non-macOS host: .app assembled from the host binary for layout/zip parity; iconutil + otool -L + the universal lipo are mac-only (SKIPPED here)"
    fi
    # Zip the .app (unsigned; first launch is right-click -> Open) and a buildable source archive.
    APPZIP="$DIST/DirWatch-${SERIAL}.app.zip"
    ( cd "$DIST" && rm -f "$(basename "$APPZIP")" && zip -q -r -y "$(basename "$APPZIP")" "DirWatch.app" )
    if [ -f "$APPZIP" ]; then pass ".app.zip written: $(basename "$APPZIP") (unsigned; right-click -> Open on first launch)"; else bad "failed to zip DirWatch.app"; fi
    SRCZIP="$DIST/DirWatch-${SERIAL}-src.zip"
    rm -f "$SRCZIP"
    # -y: store symlinks AS symlinks, never follow them — so a symlinked target/ (the checks.sh
    # gate-parity scratch links the shared build dir in) is never descended into. On the user's Mac target/
    # is a real dir and is excluded outright; the source tree has no symlinks that need following.
    ( cd "$ROOT" && zip -q -r -y "$SRCZIP" . -x './target/*' -x './target' -x './dist/*' -x 'artifacts_*.zip' -x './DirWatch_*.log' )
    if [ -f "$SRCZIP" ]; then pass "-src.zip written: $(basename "$SRCZIP")"; else bad "failed to write -src.zip"; fi
else
    bad "no dirwatch binary in dist/ - cannot assemble .app"
fi

# --- 6b. search-scan bench on hardware (.i54, R161) ----------------------------------------------
# The search-per-keystroke MEASUREMENT. Run the exe with BOTH gate env vars set; it times the real
# refresh_matches scan across buffer sizes up to the 48 MB load cap / 64 MB scrollback ceiling, writes
# BENCH lines to the per-user DirWatch.log (NO --log-dir, so § 7 collects them by build-ID filter),
# and exits without a window. Env-gated + debug-only. Kept in sync with runtests.ps1 (gate parity).
step "search-scan bench (real per-keystroke scan cost)"
if [ -x "$DIST/dirwatch" ]; then
    DIRWATCH_DEBUG=1 DIRWATCH_SEARCH_BENCH=1 "$DIST/dirwatch" >/tmp/dw_bench_run.log 2>&1
    if [ "$(uname -s)" = "Darwin" ]; then BLDIR="$HOME/Library/Logs/DirWatch"; else
        BLDIR="${XDG_STATE_HOME:-}"; case "$BLDIR" in /*) BLDIR="$BLDIR/dirwatch" ;; *) BLDIR="$HOME/.local/state/dirwatch" ;; esac
    fi
    BLOG="$BLDIR/DirWatch.log"
    if [ ! -f "$BLOG" ]; then
        bad "search-bench: no DirWatch.log at $BLDIR"
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

# --- 7. collect ONE artifact zip -----------------------------------------------------------------
step "collecting ONE artifact zip: artifacts_$SERIAL.zip"
items=("$LOG")
[ -f "$DIST/dirwatch" ] && items+=("$DIST/dirwatch")
# DirWatch.log lives in the per-user log directory since .i40 (DECISIONS R127) and persists across
# builds, so collect ONLY this build's lines (live file + its one rotated .1) into DirWatch_<serial>.log;
# the crash log is copied whole. macOS: ~/Library/Logs/DirWatch (applog.rs Os::Mac); on a non-macOS host
# (checks.sh gate parity) the Linux rule applies.
if [ "$(uname -s)" = "Darwin" ]; then
    LOGDIR="$HOME/Library/Logs/DirWatch"
else
    LOGDIR="${XDG_STATE_HOME:-}"
    case "$LOGDIR" in /*) LOGDIR="$LOGDIR/dirwatch" ;; *) LOGDIR="$HOME/.local/state/dirwatch" ;; esac
fi
DWLOG="$DIST/DirWatch_$SERIAL.log"; rm -f "$DWLOG"
for f in "$LOGDIR/DirWatch.log.1" "$LOGDIR/DirWatch.log"; do
    [ -f "$f" ] && grep -F -- "$BUILD_ID" "$f" >> "$DWLOG" 2>/dev/null
done
if [ -s "$DWLOG" ]; then
    log "  DirWatch.log: $(wc -l < "$DWLOG" | tr -d ' ') line(s) stamped $BUILD_ID collected from $LOGDIR"
    items+=("$DWLOG")
else
    log "  DirWatch.log: no lines stamped $BUILD_ID in $LOGDIR (nothing logged this build)"
    rm -f "$DWLOG"
fi
[ -f "$LOGDIR/DirWatch_crash.log" ] && items+=("$LOGDIR/DirWatch_crash.log")
for s in "$DIST"/gui_mac_*.png; do [ -f "$s" ] && items+=("$s"); done
# The 1.0 macOS ship shapes ride in the artifact zip so the user has the actual .app.zip + -src.zip (R168/R171).
for shape in "$DIST"/DirWatch-*.app.zip "$DIST"/DirWatch-*-src.zip; do [ -f "$shape" ] && items+=("$shape"); done
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
