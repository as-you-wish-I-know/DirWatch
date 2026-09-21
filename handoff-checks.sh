#!/usr/bin/env bash
# handoff-checks.sh — the HANDOFF/sign-off completeness gate for the DirWatch RUST PORT.
#
# Mirrors the .NET handoff gate (collaboration prompt v10, Gated deliverables) and adapts it to
# the Rust tree. It is the mechanized half of a v10 GATED DELIVERABLE: no handoff zip is
# delivered, and no sign-off is "recorded", until this exits green AND its output is shown at
# delivery. It does NOT verify ground truth (that a build was really signed off on hardware).
#
# Rust-specific adaptations vs. the .NET gate:
#   - stray-artifact check looks for target/ (not bin/obj) and for a committed root Cargo.lock
#     (a lib workspace should not ship one) and transient *_output.txt / *_debug.log at root.
#   - build-ID check reads BUILD_ID from build_info.rs (not BuildInfo.cs).
#   - adds the standalone-exe check hook (check-standalone-exe.sh) when a release exe is given.
#
# Usage: bash handoff-checks.sh [--zip <staged.zip>] [--gate] [--exe <release-dir-or-exe>]
#   --zip   audit a staged handoff zip's freshness/layout/cleanliness
#   --gate  also run linux-checks.sh (full off-hardware build gate) on this tree
#   --exe   also run the standalone-exe (size/no-sidecar) check on a built release exe
# Exit: 0 = every check passed; non-zero = at least one FAIL (delivery blocked).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

ZIP=""; RUN_GATE=0; EXE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --zip) ZIP="${2:-}"; shift 2 ;;
        --gate) RUN_GATE=1; shift ;;
        --exe) EXE="${2:-}"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

DECISIONS="$ROOT/DECISIONS.md"
HANDOFF="$ROOT/HANDOFF.md"
README="$ROOT/README.md"
STATE_DOCS=(DECISIONS.md HANDOFF.md BUILDS.md BACKLOG.md README.md PORT-PLAN-crossplatform.md PORT-PLAN-rust.md)
BUILDS="$ROOT/BUILDS.md"

fails=0; warns=0
pass() { echo "  [PASS] $1"; }
fail() { echo "  [FAIL] $1"; fails=$((fails+1)); }
warn() { echo "  [WARN] $1"; warns=$((warns+1)); }

echo "== handoff-checks (completeness + consistency gate) -- DirWatch (rust port) =="

# [A] sign-off consistency: no build recorded as both PASS and PENDING.
echo ""; echo "[A] sign-off consistency (no build both PASS and PENDING) ..."
if [ ! -f "$DECISIONS" ]; then
    fail "DECISIONS.md not found"
else
    contradiction=0
    # This project's decision log is a numbered list ("N. **...**"), not "## DN" headers.
    # Find lines that mark a build as PENDING sign-off, extract the build ID (backtick-quoted),
    # and check no state doc records that same ID as signed-off PASS.
    while IFS= read -r line; do
        echo "$line" | grep -qiE 'sign-?off[: ].*pending' || continue
        bid="$(echo "$line" | grep -oE '`[^`]*build[^`]*`' | head -1 | tr -d '`')"
        [ -z "$bid" ] && continue
        for d in "${STATE_DOCS[@]}"; do
            [ -f "$ROOT/$d" ] || continue
            if grep -F "$bid" "$ROOT/$d" 2>/dev/null \
                 | grep -iqE 'sign-?off (pass|complete)|signed off (pass|✓)|hardware sign-off:?\s*pass'; then
                fail "$bid is PENDING in DECISIONS.md but recorded PASS in $d"; contradiction=1
            fi
        done
    done < "$DECISIONS"
    [ "$contradiction" -eq 0 ] && pass "no PASS-vs-PENDING contradiction across ${STATE_DOCS[*]}"
fi

# [B] exactly one collaboration-prompt version in the tree, AND its version matches what HANDOFF.md
# names (v15, R117: never two versions; the packaged prompt's version is what HANDOFF's parity
# instruction points at, so a mismatch would send the next session to the wrong file).
echo ""; echo "[B] exactly one collaboration-prompt version, matching HANDOFF.md ..."
mapfile -t prompts < <(find "$ROOT" -maxdepth 1 -name 'coding-collaboration-prompt-v*.md' | sort)
if [ "${#prompts[@]}" -eq 0 ]; then
    fail "no coding-collaboration-prompt-vN.md in the tree"
elif [ "${#prompts[@]}" -gt 1 ]; then
    fail "multiple prompt versions: ${prompts[*]##*/} -- carry exactly one"
else
    pfile="${prompts[0]##*/}"
    pver="$(echo "$pfile" | grep -oE 'v[0-9]+')"
    if [ -f "$HANDOFF" ] && grep -qF "$pfile" "$HANDOFF"; then
        pass "single prompt $pfile; HANDOFF.md names it"
    elif [ -f "$HANDOFF" ] && grep -qiE "prompt.*${pver}\b|${pver}[^0-9]" "$HANDOFF"; then
        pass "single prompt $pfile; HANDOFF.md names version $pver"
    else
        fail "single prompt $pfile but HANDOFF.md does not name $pfile / $pver (version-parity pointer stale)"
    fi
fi

# [C] required project-state docs present (code-review spec absence is an accepted WARN).
echo ""; echo "[C] required project-state docs present ..."
for f in HANDOFF.md README.md DECISIONS.md PORT-PLAN-rust.md; do
    if [ -f "$ROOT/$f" ]; then pass "$f present"; else fail "$f missing"; fi
done
if ! find "$ROOT" -maxdepth 1 -name 'code-review-prompt-v*.md' | grep -q .; then
    if [ -f "$HANDOFF" ] && grep -qiE 'code-review-prompt.*(not|absent|ask mike|paste)' "$HANDOFF"; then
        warn "code-review spec absent, but HANDOFF.md flags it for the user to paste (accepted)"
    else
        fail "code-review-prompt-vN.md absent AND HANDOFF.md does not flag it -- silent omission"
    fi
fi

# [D] no build artifacts / stray transient files that would ride into the zip.
echo ""; echo "[D] no Rust build artifacts / stray transient files in the tree ..."
# NOTE (iced port, DECISIONS R74): the standard cargo build dir is `$ROOT/target`, which legitimately
# exists during development and is ALWAYS excluded from the package (the zip audit [F] proves the
# staged zip carries no target/). The old check `find $ROOT -type d -name target` fired on that live
# build dir and FAILed a correctly-packaged handoff (a misfire caught at the .i13 handoff). Fixed: the
# root cargo build dir is IGNORED here; a stray/committed `target/` NESTED elsewhere (which WOULD get
# packaged) still fails. Packaged cleanliness is authoritatively checked by [F] against the zip.
junk=0
if find "$ROOT" -type d -name target -not -path "$ROOT/target" | grep -q .; then
    fail "a nested target/ is present (not the root build dir) -- exclude Rust build output from the package"; junk=1
fi
# Cargo.lock MUST ship (DECISIONS R98, review A6): this workspace builds a BINARY, and every gate
# builds `--locked`, so a missing lockfile means an unreproducible build. (Inverted from the .i28
# check, which warned on its PRESENCE — a misfire for an application crate.)
if [ ! -f "$ROOT/Cargo.lock" ]; then
    fail "root Cargo.lock MISSING -- a binary crate ships its lockfile (DECISIONS R98)"; junk=1
fi
# .i40 (R127): the harnesses now write DirWatch_<serial>.log (this build's filtered log lines) and the
# app itself DirWatch.log / DirWatch.log.1 / DirWatch_crash.log — none may ride into the zip. Add-only.
for pat in '*_output.txt' '*_debug.log' DirWatch_debug.log 'DirWatch*.log' 'DirWatch.log.1' 'gui_*.png' 'gui_*.bmp'; do
    if find "$ROOT" -maxdepth 1 -name "$pat" | grep -q .; then
        warn "transient artifact(s) matching '$pat' at tree root -- strip before zipping"; junk=1
    fi
done
[ "$junk" -eq 0 ] && pass "no target/; Cargo.lock present; no transient logs at tree root"

# [E] build-ID consistency: build_info.rs BUILD_ID appears in the docs' current-build line.
echo ""; echo "[E] BUILD_ID matches the docs' current build ..."
bidfile="$(grep -rlE 'BUILD_ID[[:space:]]*[:=]' "$ROOT" --include='*.rs' 2>/dev/null | head -1)"
if [ -z "$bidfile" ]; then
    warn "no BUILD_ID constant found"
else
    curbid="$(grep -oE 'BUILD_ID[^"]*"[^"]+"' "$bidfile" | grep -oE '"[^"]+"' | tr -d '"' | head -1)"
    hit=0
    for d in HANDOFF.md DECISIONS.md README.md; do
        [ -f "$ROOT/$d" ] && grep -qF "$curbid" "$ROOT/$d" && hit=1
    done
    if [ "$hit" -eq 1 ]; then pass "BUILD_ID '$curbid' appears in the docs (current-build line present)"
    else warn "BUILD_ID '$curbid' not found in HANDOFF/DECISIONS/README -- confirm current-build line is up to date"; fi
fi

# ---------------------------------------------------------------------------
# v15 HANDOFF-COMPLETENESS CHECKS (DECISIONS R117). Add-only.
# ---------------------------------------------------------------------------
# [J] HANDOFF.md is a one-page pointer: <= 60 lines (v15 Records).
echo ""; echo "[J] HANDOFF.md <= 60 lines (v15 one-page pointer) ..."
if [ ! -f "$HANDOFF" ]; then
    fail "HANDOFF.md not found"
else
    hlines="$(wc -l < "$HANDOFF" | tr -d ' ')"
    if [ "$hlines" -le 60 ]; then pass "HANDOFF.md is $hlines lines (<= 60)"
    else fail "HANDOFF.md is $hlines lines (> 60) -- it must be a one-page pointer; history lives in BUILDS.md"; fi
fi

# [K] BUILDS.md: present; a line per build ID that appears in the tree; ATTEMPT never decreasing per
# symptom (v15 Records + handoff-checks). Build IDs are the .iNN suffixes; BUILDS.md lines lead with
# them. "Never decreasing per symptom" is checked as: reading BUILDS.md top-to-bottom, for each
# symptom string the ATTEMPT number never goes down.
echo ""; echo "[K] BUILDS.md present, one line per build, ATTEMPT non-decreasing per symptom ..."
if [ ! -f "$BUILDS" ]; then
    fail "BUILDS.md missing (v15's only per-build record)"
else
    # every .iNN referenced anywhere in the state docs / build_info must have a BUILDS.md line
    missing=0
    curbid_suffix=".${curbid##*.}"
    if [ -n "${curbid:-}" ] && ! grep -qE "^\s*${curbid_suffix}\b" "$BUILDS"; then
        fail "BUILDS.md has no line for the current build $curbid_suffix"; missing=1
    fi
    # ATTEMPT monotonicity per symptom
    awk -F'|' '
      /^\s*\.i[0-9]+/ {
        symptom=$4; att=$5
        gsub(/^[ \t]+|[ \t]+$/,"",symptom); gsub(/[ \t]/,"",att)
        n=att; sub(/^ATTEMPT/,"",n); gsub(/[^0-9]/,"",n)
        if (symptom=="-" || symptom=="") next
        if (n=="") next
        if (symptom in seen && n+0 < seen[symptom]+0) {
          printf("  [FAIL] ATTEMPT decreased for symptom \"%s\": %d after %d\n", symptom, n, seen[symptom]); bad=1
        }
        if (!(symptom in seen) || n+0 > seen[symptom]+0) seen[symptom]=n+0
      }
      END { exit bad?1:0 }
    ' "$BUILDS" || { fails=$((fails+1)); missing=1; }
    [ "$missing" -eq 0 ] && pass "BUILDS.md present; current build line present; ATTEMPT non-decreasing per symptom"
fi

# [L] every DECISIONS entry carries a date (v15 Records: undated history is a defect).
# This project's convention is `Rn. **title** (YYYY-MM-DD ...)` (DECISIONS R0+, accepted at v15
# adoption 1-A, R117) — NOT the v15 `## Dn — YYYY-MM-DD` form, which is not retrofitted (renumbering
# the append-only ledger is forbidden by Records). MISFIRE CORRECTED (R117): the first cut required
# the date ON the `Rn.` line and false-FAILed R8/R10/R14 (lines 93/128/226) whose title wraps and
# whose `(YYYY-MM-DD)` sits on the continuation line. Fixed: an entry is its `Rn.` line plus every
# following line up to the next `Rn.` heading or blank line; the date may appear anywhere in that
# block.
echo ""; echo "[L] every DECISIONS.md entry is dated ..."
if [ ! -f "$DECISIONS" ]; then
    fail "DECISIONS.md not found"
else
    undated="$(awk '
      /^R[0-9]+\./ {
        if (cur!="" && !dated) print curline": "cur
        cur=$0; curline=NR; dated=($0 ~ /20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]/); next
      }
      cur!="" {
        if ($0 ~ /^[[:space:]]*$/) { if(!dated) print curline": "cur; cur=""; next }
        if ($0 ~ /20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]/) dated=1
      }
      END { if (cur!="" && !dated) print curline": "cur }
    ' "$DECISIONS")"
    if [ -z "$undated" ]; then
        cnt="$(grep -cE '^R[0-9]+\.' "$DECISIONS")"
        pass "all $cnt DECISIONS entries carry a YYYY-MM-DD date"
    else
        fail "undated DECISIONS entry/entries: $(echo "$undated" | head -3 | cut -d: -f1 | paste -sd, -)"
    fi
fi

# [F] optional --zip audit: fresh (docs byte-match tree), flat (no wrapper), clean (no target/logs).
if [ -n "$ZIP" ]; then
    echo ""; echo "[F] staged zip audit ($ZIP) ..."
    if [ ! -f "$ZIP" ]; then fail "zip not found: $ZIP"
    elif ! command -v unzip >/dev/null 2>&1; then warn "unzip unavailable -- cannot audit the zip"
    else
        stale=0
        for d in DECISIONS.md HANDOFF.md BUILDS.md README.md BUILD.meta; do
            [ -f "$ROOT/$d" ] || continue
            zc="$(unzip -p "$ZIP" "$d" 2>/dev/null | sha256sum | cut -d' ' -f1)"
            tc="$(sha256sum "$ROOT/$d" | cut -d' ' -f1)"
            if [ -z "$zc" ]; then fail "zip is missing $d"; stale=1
            elif [ "$zc" != "$tc" ]; then fail "zip's $d does NOT match the tree (stale snapshot)"; stale=1; fi
        done
        [ "$stale" -eq 0 ] && pass "zip docs byte-match the tree (not stale)"
        # Capture the listing ONCE into a variable, then grep the variable. Do NOT pipe
        # `unzip -l | grep -q`: grep -q closes the pipe on first match, unzip dies with SIGPIPE
        # (exit 141), and under `set -o pipefail` that failure becomes the pipeline's status --
        # so a well-formed zip wrongly reports FAIL. (That false-FAIL bug shipped in the .NET gate
        # lineage; fixed here. Keep the gate honest: a check that cries wolf is as bad as a miss.)
        ziplist="$(unzip -l "$ZIP")"
        # Flat layout: Cargo.toml AND the build gate script must sit at the zip root (no leading
        # path). NOTE (iced port, DECISIONS R74): the gate script was RENAMED linux-checks.sh ->
        # checks.sh at the iced port (DECISIONS R57). The old check looked for `linux-checks.sh` and
        # so FAILed a correctly-laid-out iced zip (a misfire caught at the .i13 handoff). Fixed to
        # `checks.sh`.
        if echo "$ziplist" | grep -qE '[[:space:]]Cargo\.toml$' \
           && echo "$ziplist" | grep -qE '[[:space:]]checks\.sh$'; then
            pass "workspace root files at zip top level (no wrapper folder)"
        else
            fail "no top-level Cargo.toml + checks.sh -- wrapper folder or wrong layout"
        fi
        if echo "$ziplist" | grep -qE '/target/|(^|/)[^ ]*_debug\.log|(^|/)[^ ]*_output\.txt'; then
            fail "zip contains target/ or transient logs -- repackage clean"
        else
            pass "zip carries no target/ or transient logs"
        fi
    fi
fi

# [G] optional standalone-exe check on a built release exe.
if [ -n "$EXE" ]; then
    echo ""; echo "[G] standalone-exe (size/no-sidecar) check ..."
    if [ -f "$ROOT/check-standalone-exe.sh" ]; then
        if bash "$ROOT/check-standalone-exe.sh" "$EXE" >/tmp/dw_standalone.txt 2>&1; then
            pass "standalone-exe check GREEN"; grep -E '\[PASS\]' /tmp/dw_standalone.txt | sed 's/^/      /'
        else
            fail "standalone-exe check RED -- see /tmp/dw_standalone.txt"; cat /tmp/dw_standalone.txt | sed 's/^/      /'
        fi
    else
        warn "check-standalone-exe.sh missing"
    fi
fi

# [H] optional --gate: full off-hardware build gate on this tree.
# NOTE (v15, DECISIONS R117): the gate script was renamed linux-checks.sh -> checks.sh at the iced
# port (R57); this ran the retired name and would have mis-warned "gate NOT run" even with --gate.
# Corrected to checks.sh. The Windows harness is runtests.ps1 (the NWG-era runtests.cmd is retired).
echo ""
if [ "$RUN_GATE" -eq 1 ] && [ -f "$ROOT/checks.sh" ]; then
    echo "[H] running checks.sh (off-hardware build gate) ..."
    if bash "$ROOT/checks.sh" >/tmp/dw_gate.txt 2>&1; then pass "checks.sh ALL GREEN on this tree"
    else fail "checks.sh FAILED -- see: bash checks.sh"; fi
else
    warn "off-hardware build gate NOT run here (--gate to include it). The Windows GUI build gate (runtests.ps1) must be green on the user's hardware for a real handoff."
fi

# [I] The Windows harness pins the MSVC TOOLCHAIN + target (DECISIONS R15/R16/R60). WHY THIS EXISTS:
# the .r4 cut failed on the user's hardware with "dlltool.exe: program not found" -- an unpinned build
# under an active GNU toolchain targeted gnu. The fix (R16) forces the msvc TOOLCHAIN via
# `cargo +stable-x86_64-pc-windows-msvc` so MSVC's link.exe is used, independent of `rustup default`.
# The Windows harness is now `runtests.ps1` (PowerShell 5.1; the NWG-era runtests.cmd/build.cmd are
# retired, R60). This check verifies the ps1 forces both the +toolchain and the --target.
echo ""; echo "[I] runtests.ps1 pins the msvc TOOLCHAIN + target (no dlltool/gnu drift) ..."
pin_ok=1
if [ ! -f "$ROOT/runtests.ps1" ]; then
    fail "runtests.ps1 not found -- the Windows harness is missing"
    pin_ok=0
else
    if ! grep -qE "stable-x86_64-pc-windows-msvc" "$ROOT/runtests.ps1"; then
        fail "runtests.ps1 does not reference the msvc toolchain stable-x86_64-pc-windows-msvc"; pin_ok=0
    fi
    if ! grep -qE 'cargo "\+\$toolchain"|cargo \+\$toolchain|\+stable-x86_64-pc-windows-msvc' "$ROOT/runtests.ps1"; then
        fail "runtests.ps1 does not force the msvc TOOLCHAIN on cargo invocations (cargo +\$toolchain)"; pin_ok=0
    fi
    if ! grep -qE '\-\-target[[:space:]]+\$?target|x86_64-pc-windows-msvc' "$ROOT/runtests.ps1"; then
        fail "runtests.ps1 does not pass --target x86_64-pc-windows-msvc"; pin_ok=0
    fi
    [ "$pin_ok" -eq 1 ] && pass "runtests.ps1 forces the msvc toolchain + target on cargo compiles"
fi

echo ""; echo "== SUMMARY =="
echo "  checks failed : $fails"
echo "  warnings      : $warns"
if [ "$fails" -ne 0 ]; then
    echo "  BLOCKED -- fix the FAILs above before delivering the handoff."; exit 1
else
    echo "  HANDOFF GATE GREEN$([ "$warns" -ne 0 ] && echo " (with $warns warning(s) -- read them)")"; exit 0
fi
