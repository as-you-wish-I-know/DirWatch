#!/usr/bin/env bash
# make-testfiles.sh - build the files the hardware test plan opens (macOS / Linux; the Windows twin
# is make-testfiles.ps1). Idempotent: re-running recreates every file. DECISIONS R115.
#
#   bash make-testfiles.sh [directory]     (default: ./watched, created if missing)
#
# Files (sizes are exact so the plan's expectations can be checked before DirWatch is started):
#   big-multiline.log  300 MiB of typical 80-byte timestamped lines   (streaming; search: every line has "e")
#   big-oneline.log    300 MiB with NO line terminator at all          (the B12 case; printable "a" bytes)
#   big-zero.log       300 MiB of NUL bytes (a `fsutil file createnew` / preallocated file; R146)
#   blankrun.log       200 numbered lines with a 30-blank-line run after line 100
#   empty.log          0 bytes
set -euo pipefail
DIR="${1:-watched}"
mkdir -p "$DIR"
MIB=$((1024 * 1024))

LINE="2026-09-11 12:00:00.000 INFO a fairly typical log line with some text in it ok."   # 79 + \n = 80
# 300 MiB / 80 = 3932160 lines exactly (awk: on every box, no SIGPIPE games with yes|head).
awk -v n=$((300 * MIB / 80)) -v l="$LINE" 'BEGIN { for (i = 0; i < n; i++) print l }' > "$DIR/big-multiline.log"

# One line, no terminator: 300 MiB of "a" (what `fsutil file createnew` gives you, but legible
# instead of NUL bytes).
head -c $((300 * MIB)) /dev/zero | tr '\0' 'a' > "$DIR/big-oneline.log"

# The REAL fsutil/preallocated shape: 300 MiB of NUL bytes, no terminator. Review #6 finding 6
# (DECISIONS R146): each NUL renders as U+FFFD (3 bytes), so this is the input that used to hold
# 144 MB per window, over the 64 MB cap, untrimmable. The scrollback cap now bounds it by bytes.
head -c $((300 * MIB)) /dev/zero > "$DIR/big-zero.log"

{
  for i in $(seq 1 100); do printf 'line %03d\n' "$i"; done
  for i in $(seq 1 30); do printf '\n'; done
  for i in $(seq 101 200); do printf 'line %03d\n' "$i"; done
} > "$DIR/blankrun.log"

: > "$DIR/empty.log"

# Line count = newlines + 1 if the last byte is not a newline (a non-empty unterminated file is one
# line). NOT `grep -c ''`: GNU grep counts NUL bytes as line ends in a binary file, so big-zero.log
# came out as 314572800 "lines" (R146 misfire, shown at the .i48 gate).
count_lines() {
  local n; n=$(tr -cd '\n' < "$1" | wc -c | tr -d ' ')
  if [ -s "$1" ] && [ "$(tail -c1 "$1" | od -An -c | tr -d ' ')" != '\n' ]; then n=$((n+1)); fi
  echo "$n"
}
echo "wrote into $DIR:"
for f in big-multiline.log big-oneline.log big-zero.log blankrun.log empty.log; do
  p="$DIR/$f"
  bytes=$(wc -c < "$p" | tr -d ' ')
  lines=$(count_lines "$p")
  printf '  %-18s %11s bytes  %8s lines\n' "$f" "$bytes" "$lines"
done
echo "expected: big-multiline.log 314572800 bytes 3932160 lines; big-oneline.log 314572800 bytes 1 line;"
echo "          big-zero.log 314572800 bytes 1 line; blankrun.log 1830 bytes 230 lines; empty.log 0 bytes 0 lines"
