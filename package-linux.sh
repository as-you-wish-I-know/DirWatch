#!/usr/bin/env bash
# package-linux.sh - build the three Linux SHIP SHAPES and run the packaging assertions.
#
# The 1.0 Linux packaging step (DECISIONS R168), factored into ONE script that BOTH the Linux gate
# (runtests.sh, on the user's hardware) AND the off-hardware gate (checks.sh, this session's Linux host)
# source, so the shape-building + assertion logic lives in exactly one place (v15 "share scaffolding
# between sibling tools, or flag the duplication"). It is NOT a standalone gate; the caller owns
# PASS/FAIL accounting. It defines functions and, when run directly, runs them all.
#
# WHAT IT PRODUCES, into $DIST (default: ./dist):
#   1. dirwatch-<serial>-x86_64.AppImage   - the low-friction "download + chmod +x + run" shape, works
#      across distros regardless of the system glibc's *newness* relative to features it uses (it does
#      NOT bundle libc; it bundles nothing but the app + AppImage runtime - see the ldd note).
#   2. dirwatch_<ver>-1_<arch>.deb          - Debian/Ubuntu/Mint native package (cargo-deb).
#   3. dirwatch-<ver>-1.<arch>.rpm          - Fedora/RHEL/openSUSE native package (cargo-generate-rpm).
#   All three install the SAME FHS payload: /usr/bin/dirwatch + the .desktop entry + the hicolor icon
#   set (deb/rpm from crates/dirwatch/Cargo.toml [package.metadata.*]; the AppImage from an AppDir
#   assembled here). flatpak/Flathub is deferred to v1.1 (DECISIONS R168) - not built here.
#
# THE HARD ASSERTION (dispositions 2026-09-19, "no external runtime, self-contained"): `ldd` on the
# release binary shows ONLY the base set - the dynamic loader, libc, libm, libgcc_s (+ linux-vdso,
# not a real file). The GUI's GL/EGL/X11/Wayland libraries are dlopen'd AT RUNTIME by wgpu/winit and
# so never appear in `ldd` - that is expected and is exactly why we assert the LINK set is the base
# set and separately prove the GUI renders (the headless render step). A NON-base NEEDED entry - a
# bundled sidecar .so, a stray GTK/Qt link - FAILS. This is the Linux analogue of the Windows
# dumpbin/dependents assertion (no vcruntime/api-ms/ucrtbase) and the macOS otool -L assertion.
#
# PER-SHAPE LOG-DIR VERIFY (R127): a packaged build must log to the PER-USER dir
# ($XDG_STATE_HOME/dirwatch or ~/.local/state/dirwatch), never beside a read-only installed binary.
# We run the AppImage itself (extract-and-run) against an unreadable watch dir with a scratch
# HOME/XDG_STATE_HOME and assert DirWatch.log lands in the per-user dir, stamped with this build ID -
# proving the shape that runs from a read-only squashfs mount still logs correctly.
#
# TOOLCHAIN: cargo-deb, cargo-generate-rpm, and appimagetool (or its extracted mksquashfs) must be on
# PATH / locatable; each is a NAMED WARN if absent (the caller decides whether that's acceptable for
# the run), never a silent skip. On the user's Linux hardware these are one-time installs; on the
# off-hardware gate they are present in the session.
set -uo pipefail

PKG_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
DIST="${DIST:-$PKG_ROOT/dist}"
BUILD_ID="$(grep -oE 'BUILD_ID[^"]*"[^"]+"' "$PKG_ROOT/crates/dirwatch-core/src/build_info.rs" 2>/dev/null | grep -oE '"[^"]+"' | tr -d '"' | head -1)"
SERIAL="${BUILD_ID##* }"; [ -n "$SERIAL" ] || SERIAL="unknown"
VER="$(grep -m1 -E '^version = ' "$PKG_ROOT/crates/dirwatch/Cargo.toml" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')"
ARCH="$(uname -m)"
RELBIN="$PKG_ROOT/target/release/dirwatch"

# The caller may provide pass/bad/warn/log; if sourced without them (run directly), define plain ones.
if ! command -v pkg_log >/dev/null 2>&1; then
    pkg_log()  { printf '%s\n' "$*"; }
    pkg_pass() { printf '  [PASS] %s\n' "$*"; }
    pkg_bad()  { printf '  [FAIL] %s\n' "$*"; PKG_FAILS=$((PKG_FAILS+1)); }
    pkg_warn() { printf '  [WARN] %s\n' "$*"; }
fi
PKG_FAILS=0

# Locate appimagetool: a real binary on PATH, OR the extracted usr/bin/appimagetool from an
# --appimage-extract (used where FUSE is unavailable, e.g. this session). Echoes the path or nothing.
find_appimagetool() {
    if command -v appimagetool >/dev/null 2>&1; then command -v appimagetool; return; fi
    for c in "$PKG_ROOT"/appimagetool*/squashfs-root/usr/bin/appimagetool \
             /tmp/dwtools/squashfs-root/usr/bin/appimagetool \
             "$HOME"/appimagetool*/squashfs-root/usr/bin/appimagetool; do
        [ -x "$c" ] && { echo "$c"; return; }
    done
}

# ---- ASSERTION: ldd shows only the base set -----------------------------------------------------
pkg_assert_ldd_base_set() {
    pkg_log ""; pkg_log "== Linux runtime-dep assertion: ldd shows only the base set (self-contained) =="
    if [ ! -x "$RELBIN" ]; then pkg_bad "no release binary at $RELBIN - build --release first"; return; fi
    if ! command -v ldd >/dev/null 2>&1; then pkg_warn "ldd not available - runtime-dep assertion SKIPPED"; return; fi
    # Base set: the loader, libc, libm, libgcc_s, and linux-vdso (a kernel-provided pseudo-lib, no
    # file). Everything the app needs beyond these is dlopen'd at runtime (GL/X11/Wayland/portal) and
    # is NOT a link-time NEEDED entry. Any OTHER "=>" line is a non-base runtime dependency and FAILS.
    local ldd_out; ldd_out="$(ldd "$RELBIN" 2>&1)"
    printf '%s\n' "$ldd_out" | sed 's/^/    /'
    local offenders
    offenders="$(printf '%s\n' "$ldd_out" \
        | grep -E '=>|\.so' \
        | grep -vE 'linux-vdso|ld-linux|/ld-|libc\.so|libm\.so|libgcc_s\.so|libpthread\.so|libdl\.so|librt\.so' \
        | sed 's/^[[:space:]]*//')"
    if [ -n "$offenders" ]; then
        pkg_bad "ldd shows NON-base runtime dependencies (a self-contained binary must not) - offenders:"
        printf '%s\n' "$offenders" | sed 's/^/      /'
    else
        pkg_pass "ldd shows only the base set (loader + libc/libm/libgcc_s + vdso); no sidecar/toolkit .so linked"
    fi
    # Belt-and-braces: no sidecar .so shipped beside the binary in dist/ (like the existing exe check).
    if ls "$DIST"/*.so >/dev/null 2>&1; then
        pkg_bad "sidecar .so present in $DIST beside the packaged binary"
    fi
}

# ---- SHAPE: AppImage ----------------------------------------------------------------------------
pkg_build_appimage() {
    pkg_log ""; pkg_log "== Linux ship shape: AppImage =="
    if [ ! -x "$RELBIN" ]; then pkg_bad "no release binary - cannot build AppImage"; return; fi
    local AT; AT="$(find_appimagetool)"
    if [ -z "$AT" ]; then pkg_warn "appimagetool not found - AppImage SKIPPED (install it or --appimage-extract it; the user's hardware run builds it)"; return; fi
    local APPDIR OUT; APPDIR="$(mktemp -d)/DirWatch.AppDir"
    OUT="$DIST/dirwatch-${SERIAL}-${ARCH}.AppImage"
    mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
             "$APPDIR/usr/share/icons/hicolor/256x256/apps" "$APPDIR/usr/share/icons/hicolor/scalable/apps"
    cp "$RELBIN" "$APPDIR/usr/bin/dirwatch"
    cp "$PKG_ROOT/packaging/linux/dirwatch.desktop" "$APPDIR/usr/share/applications/dirwatch.desktop"
    cp "$PKG_ROOT/packaging/linux/dirwatch.desktop" "$APPDIR/dirwatch.desktop"        # AppImage: top-level .desktop
    cp "$PKG_ROOT/packaging/linux/icons/dirwatch-256.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/dirwatch.png"
    cp "$PKG_ROOT/packaging/linux/icons/dirwatch.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/dirwatch.svg"
    cp "$PKG_ROOT/packaging/linux/icons/dirwatch-256.png" "$APPDIR/dirwatch.png"      # AppImage: top-level icon
    cat > "$APPDIR/AppRun" <<'AREOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/dirwatch" "$@"
AREOF
    chmod +x "$APPDIR/AppRun"
    mkdir -p "$DIST"
    # ARCH env is how appimagetool names the runtime arch when it can't guess. Extracted mksquashfs is
    # on PATH via the caller when FUSE is unavailable.
    if ARCH="$ARCH" "$AT" "$APPDIR" "$OUT" >/tmp/dw_appimage_build.log 2>&1; then
        local sz; sz=$(stat -c%s "$OUT" 2>/dev/null || echo 0)
        if [ "$sz" -gt 100000 ]; then
            pkg_pass "AppImage built: $(basename "$OUT") ($(awk "BEGIN{printf \"%.1f\", $sz/1048576}") MB)"
        else
            pkg_bad "AppImage output too small ($sz bytes) - see /tmp/dw_appimage_build.log"
        fi
    else
        pkg_bad "appimagetool failed - see /tmp/dw_appimage_build.log"
    fi
    rm -rf "$(dirname "$APPDIR")"
}

# ---- SHAPE: .deb --------------------------------------------------------------------------------
pkg_build_deb() {
    pkg_log ""; pkg_log "== Linux ship shape: .deb (Debian/Ubuntu/Mint) =="
    if ! command -v cargo-deb >/dev/null 2>&1; then pkg_warn "cargo-deb not found - .deb SKIPPED (cargo install cargo-deb)"; return; fi
    if [ ! -x "$RELBIN" ]; then pkg_bad "no release binary - cannot build .deb"; return; fi
    mkdir -p "$DIST"
    # --no-build: reuse the release binary the harness already built + verified (one build, reused by
    # every shape - same rationale as checks.sh's shared PARITY_TARGET). Run from the crate dir.
    if ( cd "$PKG_ROOT/crates/dirwatch" && cargo deb --no-build --output "$DIST/" ) >/tmp/dw_deb_build.log 2>&1; then
        local deb; deb="$(ls "$DIST"/dirwatch_*.deb 2>/dev/null | head -1)"
        if [ -n "$deb" ] && command -v dpkg-deb >/dev/null 2>&1; then
            # Assert the binary and desktop entry landed at the FHS paths. Capture the listing ONCE
            # into a variable, then grep the variable - do NOT pipe `dpkg-deb -c | grep -q`: grep -q
            # closes the pipe on first match, dpkg-deb dies with SIGPIPE, and under `set -o pipefail`
            # that failure becomes the pipeline's status - a well-formed .deb then wrongly FAILs. (Same
            # SIGPIPE-under-pipefail trap handoff-checks.sh documents for its zip audit.)
            deb_list="$(dpkg-deb -c "$deb" 2>/dev/null)"
            if printf '%s\n' "$deb_list" | grep -q './usr/bin/dirwatch' \
               && printf '%s\n' "$deb_list" | grep -q './usr/share/applications/dirwatch.desktop'; then
                pkg_pass ".deb built + payload correct: $(basename "$deb") (Depends: $(dpkg-deb -f "$deb" Depends))"
            else
                pkg_bad ".deb built but missing /usr/bin/dirwatch or the .desktop entry - see /tmp/dw_deb_build.log"
            fi
        elif [ -n "$deb" ]; then
            pkg_pass ".deb built: $(basename "$deb") (dpkg-deb absent - contents not asserted here)"
        else
            pkg_bad "cargo-deb produced no .deb - see /tmp/dw_deb_build.log"
        fi
    else
        pkg_bad "cargo-deb failed - see /tmp/dw_deb_build.log"
    fi
}

# ---- SHAPE: .rpm --------------------------------------------------------------------------------
pkg_build_rpm() {
    pkg_log ""; pkg_log "== Linux ship shape: .rpm (Fedora/RHEL/openSUSE) =="
    if ! command -v cargo-generate-rpm >/dev/null 2>&1; then pkg_warn "cargo-generate-rpm not found - .rpm SKIPPED (cargo install cargo-generate-rpm)"; return; fi
    if [ ! -x "$RELBIN" ]; then pkg_bad "no release binary - cannot build .rpm"; return; fi
    mkdir -p "$DIST"
    # cargo-generate-rpm reads the crate's [package.metadata.generate-rpm] and the already-built
    # release binary (it never builds). -o places it in dist/.
    if cargo generate-rpm -p "$PKG_ROOT/crates/dirwatch" -o "$DIST/dirwatch-${VER}-1.${ARCH}.rpm" >/tmp/dw_rpm_build.log 2>&1; then
        local rpm; rpm="$(ls "$DIST"/dirwatch-*.rpm 2>/dev/null | head -1)"
        if [ -n "$rpm" ]; then
            local sz; sz=$(stat -c%s "$rpm" 2>/dev/null || echo 0)
            # Assert contents when rpm tooling is present (the user's Fedora/RHEL box); else assert size.
            if command -v rpm2cpio >/dev/null 2>&1 && command -v cpio >/dev/null 2>&1; then
                if rpm2cpio "$rpm" | cpio -t 2>/dev/null | grep -q 'usr/bin/dirwatch'; then
                    pkg_pass ".rpm built + payload correct: $(basename "$rpm")"
                else
                    pkg_bad ".rpm built but /usr/bin/dirwatch not in payload - see /tmp/dw_rpm_build.log"
                fi
            elif [ "$sz" -gt 100000 ]; then
                pkg_pass ".rpm built: $(basename "$rpm") ($(awk "BEGIN{printf \"%.1f\", $sz/1048576}") MB; rpm2cpio absent - contents not asserted here)"
            else
                pkg_bad ".rpm output too small ($sz bytes) - see /tmp/dw_rpm_build.log"
            fi
        else
            pkg_bad "cargo-generate-rpm produced no .rpm - see /tmp/dw_rpm_build.log"
        fi
    else
        pkg_bad "cargo-generate-rpm failed - see /tmp/dw_rpm_build.log"
    fi
}

# ---- PER-SHAPE LOG-DIR VERIFY (R127): the AppImage logs to the per-user dir, not beside the mount --
# Runs the built AppImage with a SCRATCH HOME + XDG_STATE_HOME against an unreadable watch dir (a
# guaranteed error line), headlessly, and asserts DirWatch.log appears under the per-user state dir
# stamped with this build ID - proving a shape that runs from a read-only squashfs mount still logs to
# the writable per-user location (the whole point of R127). Needs xvfb; absent => WARN.
pkg_verify_appimage_logdir() {
    pkg_log ""; pkg_log "== per-shape log-dir verify (R127): AppImage logs to the per-user dir =="
    local img; img="$(ls "$DIST"/dirwatch-*-*.AppImage 2>/dev/null | head -1)"
    if [ -z "$img" ]; then pkg_warn "no AppImage in $DIST - log-dir verify SKIPPED"; return; fi
    if ! command -v xvfb-run >/dev/null 2>&1; then pkg_warn "xvfb not available - AppImage log-dir verify SKIPPED (the user's hardware run confirms it)"; return; fi
    local SCR HOMED; SCR="$(mktemp -d)"; HOMED="$SCR/home"
    mkdir -p "$HOMED"
    export XDG_RUNTIME_DIR="$SCR/xdg"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
    export ICED_BACKEND=tiny-skia WGPU_BACKEND=gl
    (
      Xvfb :91 -screen 0 800x600x24 >/tmp/dw_ai_xvfb.log 2>&1 &
      XP=$!; export DISPLAY=:91; sleep 2
      # Fresh HOME with no XDG_STATE_HOME => the app must choose ~/.local/state/dirwatch. Extract-and-run
      # because this session has no FUSE; on the user's box the AppImage self-mounts (same code path).
      HOME="$HOMED" XDG_STATE_HOME= DIRWATCH_DEBUG=1 \
        "$img" --appimage-extract-and-run /nonexistent/dirwatch-pkg-gate-dir >/tmp/dw_ai_run.log 2>&1 &
      AP=$!; sleep 5
      kill $AP 2>/dev/null || true; kill $XP 2>/dev/null || true
    )
    local LOGF="$HOMED/.local/state/dirwatch/DirWatch.log"
    # Nothing may be written beside the (read-only) mounted binary; the AppImage mount is transient, so
    # the meaningful assertion is that the log landed in the per-user dir, stamped, with the error.
    if [ -f "$LOGF" ] && grep -qF "$BUILD_ID" "$LOGF" && grep -q "watch error" "$LOGF"; then
        pkg_pass "AppImage logged to ~/.local/state/dirwatch/DirWatch.log (stamped $BUILD_ID, watch error recorded) - not beside the read-only mount"
    else
        pkg_bad "AppImage log-dir verify failed: per-user log present=$([ -f "$LOGF" ] && echo yes || echo no) (see /tmp/dw_ai_run.log)"
    fi
    rm -rf "$SCR"
}

# Run everything when executed directly (not sourced). Sourced callers invoke the functions and own
# their own accounting.
pkg_run_all() {
    mkdir -p "$DIST"
    pkg_assert_ldd_base_set
    pkg_build_appimage
    pkg_build_deb
    pkg_build_rpm
    pkg_verify_appimage_logdir
    pkg_log ""; pkg_log "== package-linux summary: $PKG_FAILS failure(s) =="
    return "$PKG_FAILS"
}

# BASH_SOURCE[0] != $0  =>  sourced; else run directly.
if [ "${BASH_SOURCE[0]:-$0}" = "$0" ]; then
    pkg_run_all
fi
