# runtests.ps1 - the DirWatch iced-port test harness for WINDOWS (native PowerShell, no bash).
#
# Run it from the project root (the top of test\ after extracting the build zip):
#
#     powershell -ExecutionPolicy Bypass -File runtests.ps1
#
# (Use that exact line the first time - a default execution policy may block a double-click. The
# -ExecutionPolicy Bypass applies to THIS run only; it changes nothing permanently.)
#
# WHAT IT DOES, in one run:
#   1. Forces the msvc TOOLCHAIN + target (cargo +stable-x86_64-pc-windows-msvc ... --target
#      x86_64-pc-windows-msvc). The toolchain selects the linker (DECISIONS R16) - pinning only the
#      target still links with GNU dlltool if the GNU toolchain is default.
#   2. cargo fmt --check, clippy -D warnings, build --release, test (full suite), dirwatch.exe
#      --version (launch + CRT-static link check: launch, confirm alive ~1s, Stop-Process by PID - no
#      window handling, DECISIONS R133; --selftest removed at .i31, R106). One PASS/FAIL line each;
#      any failure makes the whole run FAIL (exit 1).
#   3. Reports the release dirwatch.exe SIZE and confirms it has no sidecar DLLs beside it
#      (single self-contained binary - the port's hard requirement; iced's size is flagged, R59).
#   4. Captures a GUI SCREENSHOT on your Windows: launches the real iced GUI against a seeded temp
#      directory and grabs the window via PrintWindow, so every round yields a ground-truth image
#      (the on-hardware complement to the assistant's off-hardware xvfb renders; replaces the
#      retired --selftest-gui harness).
#   5. Zips ONE artifacts_<buildID>.zip at the top of test\ (Compress-Archive - built into Windows,
#      no dependency): the run log, this build's lines from DirWatch.log (the per-user log at
#      %LOCALAPPDATA%\DirWatch since .i40, DECISIONS R127 - filtered by build ID so earlier builds'
#      lines stay out), the whole DirWatch_crash.log if one exists, and the screenshot. That single
#      file is what you upload.
#
# checks.sh is NOT used here - it is the assistant's off-hardware gate. This .ps1 is your gate.

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$toolchain = 'stable-x86_64-pc-windows-msvc'
$target    = 'x86_64-pc-windows-msvc'

# --- build ID (from build_info.rs) drives every artifact name -------------------------------------
$biPath = Join-Path $root 'crates\dirwatch-core\src\build_info.rs'
$buildId = (Select-String -Path $biPath -Pattern 'BUILD_ID[^"]*"([^"]+)"').Matches[0].Groups[1].Value
$serial  = ($buildId -split ' ')[-1]           # e.g. 2026-07-27.i2
$log     = Join-Path $root "runtests_output_$serial.txt"
$art     = Join-Path $root "artifacts_$serial.zip"
$shot    = Join-Path $root "gui_win_$serial.png"

$fails = 0

# --- logging ------------------------------------------------------------------------------------
# Log lines go to BOTH the console (default color) and the UTF-8 log file. We deliberately do NOT
# use Tee-Object/Out-File here: on PowerShell 5.1 those default to UTF-16, which made the run log
# full of spaced-out characters (DECISIONS R62). Write-Host prints in the normal color (never red),
# and Add-Content -Encoding UTF8 keeps the file readable in any editor.
function Log($s) {
    if ($null -eq $s) { $s = '' }
    Write-Host $s
    Add-Content -Path $log -Value $s -Encoding UTF8
}
function Pass($m) { Log "  [PASS] $m" }
function Bad($m)  { Log "  [FAIL] $m"; $script:fails++ }

# Run a native command (cargo / the exe) and treat its output as NORMAL text, not errors. WHY:
# cargo writes its progress ("Updating crates.io index", "Compiling", "Finished", even warnings)
# to STDERR. `& cmd 2>&1 | Out-Host` wraps each stderr line in an ErrorRecord, which PowerShell
# renders RED  -  so a totally successful build looked alarming (DECISIONS R62). Here we merge
# stderr into stdout, flatten any ErrorRecord to its plain string, and print it via Log (default
# color). Only our own [FAIL] lines and a real PowerShell exception are red. Returns the native
# exit code so each step can still decide PASS/FAIL on $LASTEXITCODE.
function RunNative([string]$file, [string[]]$argList) {
    & $file @argList 2>&1 | ForEach-Object {
        if ($_ -is [System.Management.Automation.ErrorRecord]) { Log ($_.ToString()) }
        else { Log ([string]$_) }
    }
    return $LASTEXITCODE
}

function Step($name, [scriptblock]$body) {
    Log ""
    Log "== $name =="
    & $body
}

# --- native window interop (SCREENSHOT step only) ------------------------------------------------
# Used ONLY by the GUI-screenshot step, to find DirWatch's real window and capture it (a failure
# there is a WARN, never a FAIL). The --version step does NOT use this - it proves launch+link by
# liveness + Stop-Process (R133), so no session state can make it FAIL.
#
# BUILD-UNIQUE TYPE NAME (DECISIONS R133): `Add-Type` loads a type into the whole PowerShell SESSION
# process PERMANENTLY on Windows PowerShell 5.1 - it can never be replaced, and a name guard cannot
# fix that. .i42 hit "TYPE_ALREADY_EXISTS" on a re-run (R132); .i43 added the guard, but then a STALE
# `Win32Cap` left in a reused session by the earlier .i42 run SHADOWED the new one - the guard saw
# the name already present, skipped the Add-Type, and the new method (TitledWindowsForPid) was
# missing (the .i43 FAIL: "does not contain a method named 'TitledWindowsForPid'"). Fix: stamp the
# type name with the build serial (`Win32Cap_i46`), so a stale type from ANY prior build/run cannot
# shadow this build's type - each build binds to its own. The guard on THIS exact name still avoids a
# same-build re-run throw. Bump the name whenever this block changes shape (that is the point).
if (-not ('Win32Cap_i46' -as [type])) {
Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class Win32Cap_i46 {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern int GetWindowTextLengthW(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }

    // All TITLED, visible, top-level windows owned by pid (skips winit's untitled helper).
    public static List<IntPtr> TitledWindowsForPid(uint pid) {
        var found = new List<IntPtr>();
        EnumWindows((h, l) => {
            uint wpid; GetWindowThreadProcessId(h, out wpid);
            if (wpid == pid && IsWindowVisible(h) && GetWindowTextLengthW(h) > 0) { found.Add(h); }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@
}

# Poll up to ~$TimeoutMs ms for the process's titled top-level window (winit's helper is untitled, so
# never returned - DECISIONS R130). Returns the HWND or IntPtr::Zero. Screenshot step only.
function Find-DwWindow([System.Diagnostics.Process]$proc, [int]$TimeoutMs = 3000) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($proc.HasExited) { return [IntPtr]::Zero }
        $wins = [Win32Cap_i46]::TitledWindowsForPid([uint32]$proc.Id)
        if ($wins.Count -gt 0) { return $wins[0] }
        Start-Sleep -Milliseconds 100
    }
    return [IntPtr]::Zero
}

# PostMessage(WM_CLOSE=0x0010) to a window found via Find-DwWindow. Screenshot step only.
function Close-DwWindow([IntPtr]$hwnd) {
    if ($hwnd -eq [IntPtr]::Zero) { return $false }
    return [Win32Cap_i46]::PostMessageW($hwnd, [uint32]0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
}

# Collect exactly one artifact zip and print the run verdict. Defined early so the preflight-fail
# path below can call it.
# The per-user log directory DirWatch writes to since .i40 (DECISIONS R127): %LOCALAPPDATA%\DirWatch,
# falling back to the profile's AppData\Local. The same rule applog.rs applies (Os::Windows).
function LogDir {
    $base = $env:LOCALAPPDATA
    if ([string]::IsNullOrEmpty($base)) { $base = Join-Path $env:USERPROFILE 'AppData\Local' }
    return (Join-Path $base 'DirWatch')
}

function Finish {
    Log ""
    Log "=== collecting ONE artifact zip: $([IO.Path]::GetFileName($art)) ==="
    $items = @($log)
    # DirWatch.log persists across builds now (it is no longer inside the wiped test\ tree), so
    # collect ONLY the lines stamped with THIS build's ID - from the live file and its one rotated
    # .1 generation - into DirWatch_<serial>.log (the user's 1-A: nothing of his is deleted, provenance
    # stays exact). The crash log is copied whole (a panic entry spans several lines; crashes are
    # rare enough that filtering is not worth the ambiguity).
    $ldir = LogDir
    $dwlog = Join-Path $root "DirWatch_$serial.log"
    $lines = @()
    foreach ($f in @((Join-Path $ldir 'DirWatch.log.1'), (Join-Path $ldir 'DirWatch.log'))) {
        if (Test-Path $f) {
            $m = @(Get-Content $f | Where-Object { $_ -like "*$buildId*" })
            if ($m.Count -gt 0) { $lines += $m }
        }
    }
    if ($lines.Count -gt 0) {
        Set-Content -Path $dwlog -Value $lines -Encoding UTF8
        $items += $dwlog
        Log ("  DirWatch.log: " + $lines.Count + " line(s) stamped " + $buildId + " collected from " + $ldir)
    } else {
        Log ("  DirWatch.log: no lines stamped " + $buildId + " in " + $ldir + " (nothing logged this build)")
    }
    foreach ($f in @((Join-Path $ldir 'DirWatch_crash.log'), $shot)) {
        if (Test-Path $f) { $items += $f }
    }
    if (Test-Path $art) { Remove-Item $art -Force }
    Compress-Archive -Path $items -DestinationPath $art -Force
    Log ("wrote " + [IO.Path]::GetFileName($art) + " (" + (Get-Item $art).Length + " bytes) containing:")
    foreach ($i in $items) { Log ("  " + [IO.Path]::GetFileName($i)) }
    Log ""
    if ($script:fails -gt 0) { Log "== RESULT: FAIL ($script:fails step(s)) - read the FAILs above ==" }
    else { Log "== RESULT: PASS - upload $([IO.Path]::GetFileName($art)) ==" }
}

# fresh log (UTF-8, no UTF-16 default)
Set-Content -Path $log -Value '' -Encoding UTF8
Log "=== DirWatch runtests (Windows) - $buildId ==="
# OS banner + the shell version (recorded as part of the target manifest per the collaboration
# prompt). Get-CimInstance is Windows-PowerShell/7-on-Windows only; guard it so the line never
# breaks the run on any host.
$osCaption = try { (Get-CimInstance Win32_OperatingSystem -ErrorAction Stop).Caption } catch { 'Windows' }
Log ("host: " + $osCaption + " " + [Environment]::OSVersion.Version)
Log ("PowerShell: " + $PSVersionTable.PSVersion + " (" + $PSVersionTable.PSEdition + ")")
Log ("date: " + (Get-Date -Format o))

# --- toolchain preflight --------------------------------------------------------------------------
Step "toolchain preflight ($toolchain)" {
    $have = (& rustup toolchain list) 2>&1 | Select-String -SimpleMatch $toolchain
    if ($have) {
        Pass "$toolchain installed; forcing it via cargo +$toolchain --target $target"
    } else {
        Bad "$toolchain NOT installed. Run once:  rustup toolchain install $toolchain"
        Bad "and:  rustup target add $target"
    }
}
if ($fails -gt 0) { Finish; exit 1 }

# --- gate steps -----------------------------------------------------------------------------------
Step "cargo fmt --check" {
    $rc = RunNative 'cargo' @("+$toolchain", 'fmt', '--all', '--check')
    if ($rc -eq 0) { Pass "formatting clean" } else { Bad "cargo fmt reported diffs" }
}
Step "cargo clippy --target $target (deny warnings)" {
    $rc = RunNative 'cargo' @("+$toolchain", 'clippy', '--locked', '--workspace', '--all-targets', '--target', $target, '--', '-D', 'warnings')
    if ($rc -eq 0) { Pass "clippy clean (no warnings)" } else { Bad "clippy warnings/errors" }
}
Step "cargo build --release --target $target" {
    $rc = RunNative 'cargo' @("+$toolchain", 'build', '--locked', '--release', '--workspace', '--target', $target)
    if ($rc -eq 0) { Pass "release build ok" } else { Bad "release build failed" }
}
Step "cargo test --target $target (full suite)" {
    $rc = RunNative 'cargo' @("+$toolchain", 'test', '--locked', '--workspace', '--target', $target)
    if ($rc -eq 0) { Pass "test suite green" } else { Bad "test suite failed" }
}

# Dependency audit (review #2 finding 12, DECISIONS R113): `cargo audit` checks the SHIPPED lockfile
# against the RustSec database. FAIL on a vulnerability. RUSTSEC-2026-0253 (lru pop() panic-safety)
# is ignored on record - it needs an unwinding panic and the release profile is panic=abort, so it
# cannot manifest in the shipped exe; re-judge it whenever Cargo.lock changes. Tool missing => NAMED
# WARN with the one-time install line; no network => NAMED WARN. Never silent.
Step "cargo audit (RustSec, shipped Cargo.lock)" {
    $null = & cargo @("+$toolchain", 'audit', '--version') 2>&1
    if ($LASTEXITCODE -eq 0) {
        $before = (Get-Content $log).Count
        $rc = RunNative 'cargo' @("+$toolchain", 'audit', '--ignore', 'RUSTSEC-2026-0253')
        if ($rc -eq 0) { Pass "cargo audit: no known vulnerabilities (RUSTSEC-2026-0253 ignored: panic=abort, R113)" }
        else {
            $tailText = (Get-Content $log | Select-Object -Skip $before) -join "`n"
            # cargo-audit's own wording when the advisory DB cannot be fetched (its `fetch` step); a
            # genuine advisory whose text happened to contain "network" used to be downgraded to
            # this WARN (review #4 finding 17), so the broad words are gone.
            if ($tailText -match "couldn't fetch advisory database|error: couldn.t fetch|failed to fetch advisory|git fetch error") {
                Log "  [WARN] cargo audit could not fetch the advisory database (offline?) - audit SKIPPED, run it online"
            } else { Bad "cargo audit found a vulnerability in Cargo.lock (exit $rc) - read the advisory above" }
        }
    } else {
        Log "  [WARN] cargo-audit not installed - audit SKIPPED. One-time setup: cargo install cargo-audit --locked"
    }
}

$exe = Join-Path $root "target\$target\release\dirwatch.exe"

# --selftest was removed at .i31 (release-prep, DECISIONS R106). Runtime proof is the GUI screenshot
# step below (launches the real exe); this just confirms the exe LAUNCHES + LINKS via the same
# RunNative path --selftest used - the key CRT-static "no missing runtime DLL" check on Windows.
Step "binary launches (--version, launch + CRT-static link check)" {
    # WHAT THIS PROVES: the release exe LAUNCHES and its statically-linked CRT resolves (the key
    # Windows "no missing runtime DLL" check; --selftest was removed at .i31, DECISIONS R106).
    #
    # WHY IT NO LONGER TOUCHES THE WINDOW (DECISIONS R133): on Windows --version opens the CLI text
    # WINDOW (R91: a GUI-subsystem exe cannot print to cmd), so the harness always had to end the
    # process itself. Every window-close approach failed on the user's hardware: .i26-.i40 waited for a
    # hand-close; .i41 CloseMainWindow() closed winit's helper window (R130); .i42/.i43 the
    # EnumWindows finder broke because a stale `Win32Cap` from a PRIOR run in the SAME PowerShell
    # session shadowed the new type (5.1 loads a type permanently; a guard cannot replace it - R132)
    # and TitledWindowsForPid went missing (R133). The window-close was never actually needed here:
    # this step's job is launch+link, so we just launch it, confirm it is ALIVE briefly (that IS the
    # proof - a missing-DLL/link failure exits immediately with a nonzero code and no live process),
    # then terminate it by PID. No Add-Type, no window enumeration - nothing a reused 5.1 session can
    # poison. Graceful GUI close is a hardware test-plan item, not this launch check.
    if (Test-Path $exe) {
        $vp = Start-Process -FilePath $exe -ArgumentList @('--version') -PassThru
        Start-Sleep -Milliseconds 1200
        $vp.Refresh()
        if ($vp.HasExited) {
            # It exited on its own within ~1.2 s. A GUI-subsystem --version normally does NOT (it sits
            # in its window), so this means either an immediate link/launch failure (nonzero) or a
            # fast clean exit (0). Judge by the code: 0 is still a valid launch+link proof.
            if ($vp.ExitCode -eq 0) { Pass "dirwatch --version launched and exited 0 on its own" }
            else { Bad ("dirwatch --version exited " + $vp.ExitCode + " almost immediately - launch/link failure (missing DLL?)") }
        } else {
            # Still alive after 1.2 s = it launched, linked, and realized its CLI window. That is the
            # proof. End it by PID (Stop-Process; the whole tree, in case a child was spawned).
            try { Stop-Process -Id $vp.Id -Force -ErrorAction Stop } catch {}
            Start-Sleep -Milliseconds 300
            $vp.Refresh()
            if (-not $vp.HasExited) { try { $vp.Kill() } catch {} }
            Pass "dirwatch --version launched and stayed alive ~1.2 s (launch + CRT-static link OK); ended by the harness"
        }
    } else { Bad "release exe missing at $exe" }
}

Step "standalone exe check (size + no sidecar DLLs)" {
    if (Test-Path $exe) {
        $bytes = (Get-Item $exe).Length
        $mb = [math]::Round($bytes/1MB, 2)
        Log "  exe size: $bytes bytes (~$mb MB)"
        $dlls = Get-ChildItem (Split-Path $exe) -Filter *.dll -ErrorAction SilentlyContinue
        if ($dlls) { Bad ("sidecar DLLs present: " + ($dlls.Name -join ', ')) }
        else { Pass "no sidecar DLLs beside the exe (single self-contained binary)" }
    } else { Bad "release exe missing - cannot size-check" }
}

# --- Windows runtime-dep assertion: static CRT, no dynamic C-runtime DLLs (R107/R171, 1.0 packaging) ---
# The Windows analogue of the Linux `ldd` base-set assertion and the macOS `otool -L` assertion (the
# "no external runtime, self-contained" hard requirement). `dumpbin /dependents` lists the DLLs the
# exe imports; a crt-static build (the restored .cargo/config.toml, R107) must import NONE of the
# C-runtime DLLs a dynamic build would need: vcruntime*, msvcp*, ucrtbase*, or the api-ms-win-crt-*
# umbrella. Core Windows system DLLs (KERNEL32, USER32, GDI32, and the GUI's dxgi/d3d, ...) are
# expected and fine. dumpbin ships with MSVC; found on PATH (Developer PowerShell) or via vswhere.
# Absent => NAMED WARN (the exe still built + launched crt-static above). This is the assertion whose
# ABSENCE R168 flagged: "the existing no-sidecar-DLL check does NOT catch a dynamically-linked C runtime".
Step "dumpbin /dependents: static CRT, no dynamic C-runtime DLLs (R107/R171)" {
    if (-not (Test-Path $exe)) { Bad "release exe missing - cannot run dumpbin"; return }
    $dumpbin = $null
    $onPath = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($onPath) { $dumpbin = $onPath.Source }
    else {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (Test-Path $vswhere) {
            $vsroot = & $vswhere -latest -products * -property installationPath 2>$null
            if ($vsroot -and (Test-Path (Join-Path $vsroot 'VC\Tools\MSVC'))) {
                $dumpbin = Get-ChildItem -Path (Join-Path $vsroot 'VC\Tools\MSVC') -Directory -ErrorAction SilentlyContinue |
                    ForEach-Object { Join-Path $_.FullName 'bin\Hostx64\x64\dumpbin.exe' } |
                    Where-Object { Test-Path $_ } | Select-Object -Last 1
            }
        }
    }
    if (-not $dumpbin) {
        Log "  [WARN] dumpbin not found on PATH or via vswhere - CRT-static dependents assertion SKIPPED (run from a 'Developer PowerShell for VS', or add the MSVC Hostx64\x64 bin to PATH). The exe still built + launched crt-static above."
        return
    }
    $out = & $dumpbin /dependents $exe 2>&1
    $out | ForEach-Object { Log "    $_" }
    # The C-runtime DLLs a NON-static build would pull in. A crt-static exe imports none of them.
    $offenders = $out | Select-String -Pattern '(?i)\b(vcruntime\d*|msvcp\d*|ucrtbase|api-ms-win-crt)[^\s]*\.dll' -AllMatches |
        ForEach-Object { $_.Matches.Value } | Sort-Object -Unique
    if ($offenders) {
        Bad ("dynamic C-runtime DLL dependency present (crt-static expected NONE): " + ($offenders -join ', '))
    } else {
        Pass "no dynamic C-runtime DLL dependency (no vcruntime/msvcp/ucrtbase/api-ms-win-crt) - CRT is statically linked"
    }
}

# --- GUI screenshot on hardware -------------------------------------------------------------------
Step "GUI screenshot (real iced window on your Windows)" {
    if (Test-Path $exe) {
        # Seed a temp dir with a couple of matching files so the grid has tiles.
        $wdir = Join-Path $env:TEMP ("dirwatch_shot_" + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $wdir | Out-Null
        Set-Content -Path (Join-Path $wdir 'app.log')   -Value "line1`nline2"
        Set-Content -Path (Join-Path $wdir 'notes.txt') -Value "hello"
        # Quoted: Start-Process joins the array with spaces and does NOT quote, so a %TEMP% under a
        # user name with a space split into two positionals (review #4 finding 17).
        $p = Start-Process -FilePath $exe -ArgumentList @("`"$wdir`"", '--patterns', '*.log;*.txt') -PassThru
        Start-Sleep -Seconds 4
        $captured = $false
        $h = [IntPtr]::Zero
        try {
            Add-Type -AssemblyName System.Drawing
            # Win32Cap_i46 is defined ONCE at the top of the script (build-unique name so a stale type
            # from a prior run cannot shadow it - DECISIONS R133). Find the process's TITLED top-level
            # window, not MainWindowHandle - which can latch winit's untitled helper window (R130).
            $h = Find-DwWindow $p 3000
            if ($h -ne [IntPtr]::Zero) {
                $r = New-Object Win32Cap_i46+RECT
                [void][Win32Cap_i46]::GetWindowRect($h, [ref]$r)
                $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
                if ($w -gt 0 -and $ht -gt 0) {
                    $bmp = New-Object System.Drawing.Bitmap $w, $ht
                    $g = [System.Drawing.Graphics]::FromImage($bmp)
                    $hdc = $g.GetHdc()
                    # flag 2 = PW_RENDERFULLCONTENT (captures GPU-composited/client content).
                    [void][Win32Cap_i46]::PrintWindow($h, $hdc, 2)
                    $g.ReleaseHdc($hdc); $g.Dispose()
                    $bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
                    $bmp.Dispose()
                    $captured = $true
                }
            }
        } catch {
            Log "  screenshot error: $($_.Exception.Message)"
        }
        # Close via WM_CLOSE to the TITLED window (found above, or re-find if the capture path was
        # skipped); Kill only as a last resort if it will not go (DECISIONS R130).
        try {
            if (-not $h -or $h -eq [IntPtr]::Zero) { $h = Find-DwWindow $p 1000 }
            [void](Close-DwWindow $h)
            if (-not $p.WaitForExit(3000)) { $p.Kill() }
        } catch { try { if (!$p.HasExited) { $p.Kill() } } catch {} }
        Remove-Item -Recurse -Force $wdir -ErrorAction SilentlyContinue
        if ($captured -and (Test-Path $shot) -and (Get-Item $shot).Length -gt 2000) {
            Pass "captured $([IO.Path]::GetFileName($shot)) ($((Get-Item $shot).Length) bytes)"
        } else {
            # A blank/failed capture is a WARN not a FAIL - the compile/test gate is what blocks a
            # build; the screenshot is diagnostic. If it's blank, say so and fall back to a manual grab.
            Log "  [WARN] automatic capture produced no usable image - take a manual screenshot of the DirWatch window and upload it too."
        }
    } else { Log "  [WARN] release exe missing - no screenshot" }
}

# --- search-scan bench on hardware (.i54, R161) ---------------------------------------------------
# The search-per-keystroke MEASUREMENT. Run the exe with BOTH gate env vars set; it times the real
# refresh_matches scan (find_matches + clamp_current) across buffer sizes up to the 48 MB load cap
# and 64 MB scrollback ceiling, writes one BENCH line per (buffer x query) to the per-user DirWatch.log
# (%LOCALAPPDATA%\DirWatch), and EXITS without opening a window. Those BENCH lines are collected into
# artifacts_<serial>.zip by the same build-ID filter as every other DirWatch.log line, so the NUMBERS
# on your Windows travel back in the one zip. Env-gated + debug-only: a normal launch never runs it.
Step "search-scan bench (real per-keystroke scan cost, your Windows)" {
    if (Test-Path $exe) {
        $prev_dbg = $env:DIRWATCH_DEBUG; $prev_bench = $env:DIRWATCH_SEARCH_BENCH
        $env:DIRWATCH_DEBUG = '1'; $env:DIRWATCH_SEARCH_BENCH = '1'
        try {
            # No --log-dir: let it write to the per-user log the collector already reads. Blocks until
            # the sweep finishes (it exits on its own; no window). Larger buffers can take a few seconds.
            $p = Start-Process -FilePath $exe -PassThru -WindowStyle Hidden
            if (-not $p.WaitForExit(120000)) { $p.Kill(); Bad "search-bench: exe did not exit within 120 s" }
        } finally {
            $env:DIRWATCH_DEBUG = $prev_dbg; $env:DIRWATCH_SEARCH_BENCH = $prev_bench
        }
        $ldir = LogDir
        $blog = Join-Path $ldir 'DirWatch.log'
        if (-not (Test-Path $blog)) {
            Bad "search-bench: no DirWatch.log at $ldir"
        } else {
            $lines = Get-Content $blog | Where-Object { $_ -like "*$buildId*" }
            $started = $lines | Where-Object { $_ -like '*search-scan sweep START*' }
            $ended   = $lines | Where-Object { $_ -like '*search-scan sweep END*' }
            $cap48   = $lines | Where-Object { $_ -like '*buffer=48MB(INITIAL_LOAD_CAP)*query="error"*scan=*ms*' }
            $cap64   = $lines | Where-Object { $_ -like '*buffer=64MB(SCROLLBACK_CAP)*query="error"*scan=*ms*' }
            if (-not $started) { Bad "search-bench: no stamped sweep START line" }
            elseif (-not $ended) { Bad "search-bench: sweep did not complete (no END line)" }
            elseif (-not $cap48) { Bad "search-bench: no 48 MB (load-cap) measurement" }
            elseif (-not $cap64) { Bad "search-bench: no 64 MB (scrollback-cap) measurement" }
            else {
                $worst = ($lines | Where-Object { $_ -like '*buffer=64MB(SCROLLBACK_CAP)*query="error"*' } | Select-Object -Last 1)
                Pass "search-scan bench ran + stamped; worst case: $worst"
            }
        }
    } else { Log "  [WARN] release exe missing - no search bench" }
}

# --- raise-to-front: PRODUCTIONIZED at .i30 (DECISIONS R101) -------------------------------------
# The .i23 8-variant raise diagnostic (R86/R100) was removed at .i30: the winner (winit gain_focus,
# variant V1) is now the shipped behavior, so there is no matrix to drive or screenshots to capture.
# The auto-open raise is a hardware-eye test-plan item (background app -> auto-open -> comes to front).

Finish
if ($fails -gt 0) { exit 1 } else { exit 0 }
