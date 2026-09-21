# make-testfiles.ps1 - build the files the hardware test plan opens (Windows PowerShell 5.1; the
# macOS/Linux twin is make-testfiles.sh). Idempotent: re-running recreates every file. DECISIONS R115.
#
#   powershell -ExecutionPolicy Bypass -File make-testfiles.ps1 [-Dir <directory>]
#   (default: .\watched, created if missing)
#
# Files (sizes are exact so the plan's expectations can be checked before DirWatch is started):
#   big-multiline.log  300 MiB of typical 80-byte timestamped lines   (streaming; search: every line has "e")
#   big-oneline.log    300 MiB with NO line terminator at all          (the B12 case; printable "a" bytes)
#   big-zero.log       300 MiB of NUL bytes (a `fsutil file createnew` / preallocated file; R146)
#   blankrun.log       200 numbered lines with a 30-blank-line run after line 100
#   empty.log          0 bytes
# Written with raw byte streams (ASCII, LF line endings) so the files are byte-identical to the
# .sh twin's - no PowerShell Out-File/Set-Content encoding or CRLF surprises.
param([string]$Dir = "watched")
$ErrorActionPreference = 'Stop'
if (!(Test-Path -LiteralPath $Dir)) { New-Item -ItemType Directory -Path $Dir | Out-Null }
$Dir = (Resolve-Path -LiteralPath $Dir).Path
$MiB = 1024 * 1024
$enc = [System.Text.Encoding]::ASCII

function Write-Repeated([string]$path, [byte[]]$unit, [long]$total) {
    # Stream `unit` until exactly `total` bytes are written (unit divides total for our sizes).
    $fs = [System.IO.File]::Create($path)
    try {
        # Build a ~1 MiB block of repeated units, then write whole blocks and one remainder.
        $reps = [Math]::Max(1, [int]([Math]::Floor($MiB / $unit.Length)))
        $block = New-Object byte[] ($unit.Length * $reps)
        for ($i = 0; $i -lt $reps; $i++) { [Array]::Copy($unit, 0, $block, $i * $unit.Length, $unit.Length) }
        $left = $total
        while ($left -ge $block.Length) { $fs.Write($block, 0, $block.Length); $left -= $block.Length }
        if ($left -gt 0) { $fs.Write($block, 0, [int]$left) }
    } finally { $fs.Close() }
}

# 79 chars + LF = 80 bytes; 300 MiB / 80 = 3932160 lines exactly.
$line = $enc.GetBytes("2026-09-11 12:00:00.000 INFO a fairly typical log line with some text in it ok.`n")
Write-Repeated (Join-Path $Dir 'big-multiline.log') $line (300 * $MiB)

# One line, no terminator: 300 MiB of "a" (what `fsutil file createnew` gives you, but legible
# instead of NUL bytes).
Write-Repeated (Join-Path $Dir 'big-oneline.log') $enc.GetBytes('a') (300 * $MiB)

# The REAL fsutil/preallocated shape: 300 MiB of NUL bytes, no terminator (review #6 finding 6,
# DECISIONS R146): each NUL renders as U+FFFD (3 bytes), the input the scrollback cap now bounds by bytes.
Write-Repeated (Join-Path $Dir 'big-zero.log') (New-Object byte[] 1) (300 * $MiB)

$sb = New-Object System.Text.StringBuilder
for ($i = 1; $i -le 100; $i++) { [void]$sb.Append(('line {0:d3}' -f $i) + "`n") }
for ($i = 1; $i -le 30; $i++) { [void]$sb.Append("`n") }
for ($i = 101; $i -le 200; $i++) { [void]$sb.Append(('line {0:d3}' -f $i) + "`n") }
[System.IO.File]::WriteAllBytes((Join-Path $Dir 'blankrun.log'), $enc.GetBytes($sb.ToString()))

[System.IO.File]::WriteAllBytes((Join-Path $Dir 'empty.log'), (New-Object byte[] 0))

Write-Host "wrote into ${Dir}:"

# Verify a file's LAST bytes only (no per-byte scan). R118 (review-walk finding): the pre-.i34
# report counted LF bytes across the whole file - ~600M PowerShell loop iterations for the two
# 300 MB files, 10-20 min (the user Ctrl-C'd it). The files were always correct; only the REPORT was
# slow. Now: size comes from the file attribute, line counts are stated BY CONSTRUCTION (this script
# wrote exactly these), and each big file is validated by reading only its final bytes.
function Test-TailBytes([string]$path, [byte[]]$want) {
    $fs = [System.IO.File]::OpenRead($path)
    try {
        $len = $fs.Length
        if ($len -lt $want.Length) { return $false }
        [void]$fs.Seek($len - $want.Length, [System.IO.SeekOrigin]::Begin)
        $buf = New-Object byte[] $want.Length
        [void]$fs.Read($buf, 0, $want.Length)
    } finally { $fs.Close() }
    for ($i = 0; $i -lt $want.Length; $i++) { if ($buf[$i] -ne $want[$i]) { return $false } }
    return $true
}

# name; expected size (attribute); line count BY CONSTRUCTION; tail bytes to validate (or $null).
$expect = @(
    @{ name = 'big-multiline.log'; bytes = 314572800L; lines = 3932160; tail = $line },
    @{ name = 'big-oneline.log';   bytes = 314572800L; lines = 1;       tail = $enc.GetBytes('a') },
    @{ name = 'big-zero.log';      bytes = 314572800L; lines = 1;       tail = (New-Object byte[] 1) },
    @{ name = 'blankrun.log';      bytes = 1830L;      lines = 230;     tail = $null },
    @{ name = 'empty.log';         bytes = 0L;         lines = 0;       tail = $null }
)
$allok = $true
foreach ($e in $expect) {
    $p = Join-Path $Dir $e.name
    $bytes = (Get-Item -LiteralPath $p).Length
    $sizeok = ($bytes -eq $e.bytes)
    $tailok = $true
    $tailtxt = '-'
    if ($null -ne $e.tail) {
        $tailok = Test-TailBytes $p $e.tail
        $tailtxt = if ($tailok) { 'ok' } else { 'BAD' }
    }
    if ($sizeok -and $tailok) { $status = 'OK' } else { $status = 'FAIL'; $allok = $false }
    Write-Host ('  {0,-18} {1,11} bytes  {2,8} lines (by construction)  tail:{3,-3}  [{4}]' -f `
            $e.name, $bytes, $e.lines, $tailtxt, $status)
}
if (-not $allok) { throw "make-testfiles.ps1: a file did not match its expected size or tail bytes" }
