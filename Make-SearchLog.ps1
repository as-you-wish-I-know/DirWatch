<#
.SYNOPSIS
  Generate a large, SEARCHABLE .log file to exercise DirWatch's tail + search (incl. the .i55
  debounce, which kicks in above a 4 MB buffer). Unlike watched\big.log (300 MB of NUL bytes,
  nothing to find), this file is real multi-line text with INFO/DEBUG/WARN/ERROR lines so there
  is plenty to search for.

.PARAMETER Path
  Output file path. Default: .\search_big.log

.PARAMETER SizeMB
  Approximate target size in megabytes. Default: 64 (DirWatch keeps at most ~48-64 MB of a file's
  tail in memory, so 64+ guarantees the in-memory buffer is above the 4 MB debounce threshold).

.EXAMPLE
  .\Make-SearchLog.ps1                      # 64 MB search_big.log in the current folder
  .\Make-SearchLog.ps1 -SizeMB 200 -Path .\watched\search_big.log

.NOTES
  Known searchable tokens (case-insensitive): error, warn, info, debug, request, cache, orders,
  upstream, latency. Roughly: ERROR ~2% of lines, WARN ~10%, DEBUG ~30%, INFO the rest.
#>
[CmdletBinding()]
param(
    [string]$Path = ".\search_big.log",
    [int]$SizeMB = 64
)

$target = [int64]$SizeMB * 1MB

# Resolve a relative $Path against PowerShell's CURRENT LOCATION ($PWD), not the .NET process
# working directory. [System.IO.StreamWriter] resolves a relative path against
# [Environment]::CurrentDirectory, which in PowerShell is usually the shell's STARTUP directory,
# NOT $PWD after a Set-Location/cd. That mismatch wrote search_big.log to the wrong folder and then
# `Get-Item $Path` (which does honour $PWD) failed with "Cannot find path". Make the path absolute
# and both sides agree. Resolve-Path can't be used (the file may not exist yet), so join manually.
if (-not [System.IO.Path]::IsPathRooted($Path)) {
    $Path = [System.IO.Path]::GetFullPath((Join-Path (Get-Location).ProviderPath $Path))
}

$templates = @(
    '2026-09-18 12:{0:D2}:{1:D2} INFO   request handled ok id={2} latency=12ms path=/api/v1/items',
    '2026-09-18 12:{0:D2}:{1:D2} DEBUG  cache hit for key user:{2} ttl=300',
    '2026-09-18 12:{0:D2}:{1:D2} WARN   slow query took 812ms on table orders id={2}',
    '2026-09-18 12:{0:D2}:{1:D2} ERROR  failed to connect to upstream service id={2} retrying'
)

# Buffered writer so a big file is fast (no per-line flush).
$sw = [System.IO.StreamWriter]::new($Path, $false, [System.Text.Encoding]::ASCII)
try {
    $written = [int64]0
    $i = 0
    while ($written -lt $target) {
        if     ($i % 50 -eq 0) { $t = $templates[3] }   # ERROR ~2%
        elseif ($i % 10 -eq 0) { $t = $templates[2] }   # WARN ~10%
        elseif ($i % 3  -eq 0) { $t = $templates[1] }   # DEBUG
        else                   { $t = $templates[0] }   # INFO
        $line = [string]::Format($t, [int](($i / 60) % 60), [int]($i % 60), $i)
        $sw.WriteLine($line)
        $written += $line.Length + 2   # + CRLF
        $i++
    }
}
finally { $sw.Close() }

$size = (Get-Item $Path).Length
Write-Host ("Wrote {0} ({1:N1} MB, {2:N0} lines)." -f $Path, ($size/1MB), $i)
Write-Host "Searchable tokens: error warn info debug request cache orders upstream latency"
