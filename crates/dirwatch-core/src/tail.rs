//! Incremental file tailer. Ported from the .NET `TailReader` (behavioral spec, build
//! 2026-07-14.9).
//!
//! Reads from the beginning on first sight then follows appended bytes. Detects
//! truncation/rotation (length shrank → re-read from start) and deletion/reappearance. Encoding
//! is detected once per (re)start and decoded with a STATEFUL decoder so multi-byte characters
//! split across reads are handled correctly.
//!
//! THE INVARIANT THAT BIT THE .NET BUILD ONCE (regression `multibyte_char_split_across_polls`):
//! feed bytes to the decoder even when a chunk yields zero complete chars, so an incomplete
//! trailing multi-byte sequence is BUFFERED for the next poll instead of dropped. encoding_rs's
//! `Decoder` does this: called with `last = false`, it retains a partial trailing sequence in
//! its own internal state across calls.
//!
//! ERRORS ARE REPORTED, NEVER SWALLOWED (review #4 finding 2, DECISIONS R128): every seek/read
//! failure surfaces in [`TailResult::error`] (the runtime edge-triggers it into one marker) and
//! clears [`TailResult::more`], so a persistently unreadable file can never spin the reader thread.

use crate::encoding;
use encoding_rs::{CoderResult, Decoder};
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const CHUNK_SIZE: usize = 8192;

/// How many bytes of a file's head to fingerprint for in-place-rotation detection (review B9,
/// DECISIONS R102). Copy-truncate rotation refills a file to the SAME-or-LARGER size in one poll
/// interval, so `len < position` (the truncation-shrank test) never fires; the head bytes change
/// instead. 64 bytes catches a new log's first timestamp. A log whose files share a CONSTANT header
/// longer than that is caught by the file-identity check when the file OBJECT changed (review #4
/// finding 3, R128) — and, since `.i48`, by the CURSOR fingerprint below when it did not: on Linux
/// a delete-and-recreate reuses the inode, so identity alone let the new file's first bytes be
/// skipped (review #6 finding 4, R148). The same window size is used for both fingerprints.
const HEAD_FINGERPRINT_LEN: usize = 64;

/// In-memory tail scrollback bound, in bytes (BACKLOG §4, DECISIONS R104; moved here from the GUI
/// at .i32, review #2 finding 5, so the READER that produces the first chunk knows the bound too).
/// The GUI trims its buffer past this to [`SCROLLBACK_TRIM_TO`]; the reader never loads more than
/// [`INITIAL_LOAD_CAP`] of an existing file's history, so a 2 GB log opens with its last 48 MB.
pub const SCROLLBACK_CAP_BYTES: usize = 64 * 1024 * 1024;
/// After a trim, the GUI drops down to this so trimming is amortized (one trim per ~16 MB appended
/// past the cap, not one per tick at the ceiling).
pub const SCROLLBACK_TRIM_TO: usize = SCROLLBACK_CAP_BYTES * 3 / 4;
/// How much of an EXISTING file's history the reader loads at first sight (and after a rotation or
/// reappearance that restarts from the top): the trim target, so the first load never triggers an
/// immediate trim. Bytes before it are skipped, reported once via [`TailResult::skipped_bytes`].
pub const INITIAL_LOAD_CAP: u64 = SCROLLBACK_TRIM_TO as u64;
/// Most bytes one [`TailReader::poll`] decodes before returning with [`TailResult::more`] set, so a
/// large history STREAMS to the caller in bounded pieces (review #2 finding 5) instead of landing as
/// one file-sized chunk — and a caller that wants to stop can stop between pieces.
pub const MAX_BYTES_PER_POLL: u64 = 4 * 1024 * 1024;
/// Polls between forced head-fingerprint / identity re-checks while the length is unchanged (review
/// #2 finding 7): the head is normally checked only when the length changes (an in-place rotation to
/// a different size), so an idle file costs one `metadata` per poll, not a seek+read too; a
/// rotation to EXACTLY the same size is still caught within this many polls (~2.5 s at the 250 ms
/// tail poll — the tail poll became the watch poll interval at R144, default 250 ms, not 100 ms).
const HEAD_RECHECK_POLLS: u32 = 10;
/// How many polls a file first seen with 1–3 bytes, NO byte-order mark, and a NUL byte among them
/// (so it may be UTF-16) is held back before the reader commits to an encoding (review #4 finding
/// 14, DECISIONS R128). Encoding detection needs ≥ 4 bytes for its no-BOM UTF-16 heuristic;
/// committing UTF-8 on a 2-byte first sight froze a UTF-16 writer's file as `A\0B\0…` for its life.
/// ~2.5 s at the 250 ms tail poll (the tail poll follows the watch poll interval since R144, default
/// 250 ms, not 100 ms); a file that stays that tiny is then committed as UTF-8 (the only honest
/// guess) so it never sits on "loading…". Tiny files without a NUL commit immediately.
pub const SHORT_HEAD_DEFER_POLLS: u32 = 10;

/// Outcome of a single [`TailReader::poll`].
#[derive(Debug, Default, Clone)]
pub struct TailResult {
    /// Newly decoded text this poll, or `None` if nothing new / not decodable yet.
    pub new_text: Option<String>,
    pub rotated: bool,
    pub reappeared: bool,
    pub missing: bool,
    pub exists: bool,
    pub encoding_label: String,
    /// The file exists (or its existence is unknown) but could not be opened/stat'ed/READ this
    /// poll — a sharing violation, permission denied, a byte-range lock, a share that timed out.
    /// Carries the OS error text. Set on EVERY failing poll (like `missing`); the runtime
    /// edge-triggers it for display (review B2, DECISIONS R97; read errors since review #4 finding
    /// 2). Any text decoded BEFORE the failure in the same poll is still delivered in `new_text`.
    pub error: Option<String>,
    /// This poll started reading an existing file from the top but the file was longer than
    /// [`INITIAL_LOAD_CAP`]: this many leading bytes were skipped (the read began at the first line
    /// boundary after them). Set once, on the (re)start poll. Review #2 finding 5.
    pub skipped_bytes: Option<u64>,
    /// More bytes were available than one poll decodes ([`MAX_BYTES_PER_POLL`]); the caller should
    /// poll again immediately (a stop check in between is the point). Review #2 finding 5. NEVER
    /// set together with `error`, and never set unless this poll actually advanced (review #4
    /// finding 2) — so a caller that loops on `more` cannot spin.
    pub more: bool,
    /// The reader has completed its FIRST poll of a present file (set on that poll only), whether or
    /// not it produced text — so a caller can drop a "loading…" state for an EMPTY file too
    /// (review #2 finding 4) and take `encoding_label` even when nothing was decoded.
    pub ready: bool,
}

/// The identity of the file object behind the path, independent of its content (review #4 finding
/// 3, DECISIONS R128/R129): a rename-and-recreate or a restart that rewrites the same path produces a
/// DIFFERENT object even when its first 64 bytes are identical (a fixed startup banner, a W3C/IIS
/// `#Software:` header), which the head fingerprint alone cannot tell from a plain append. Unix:
/// `(device, inode)`. Windows: `(volume serial, 64-bit file index)` from `GetFileInformationByHandle`
/// — the MFT file reference including its sequence number, which a new file object always gets
/// fresh. NOT the creation time: `.i40` used that and the user's item-10 test showed NTFS "tunneling"
/// hands a file recreated under the same name within ~15 s of the rename the OLD creation time — and
/// rename-then-recreate-immediately is exactly how loggers rotate, so the check never fired (R129).
/// If the Win32 call ever fails the creation time is the fallback. Other platforms: no identity
/// (`None` never differs from `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId(u64, u64);

#[cfg(unix)]
fn file_id(_fs: &File, m: &Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some(FileId(m.dev(), m.ino()))
}

#[cfg(windows)]
fn file_id(fs: &File, m: &Metadata) -> Option<FileId> {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;

    // `BY_HANDLE_FILE_INFORMATION` (winbase.h): layout fixed since Windows 95 — 52 bytes, all
    // 32-bit fields, `FILETIME` = two u32. Declared by hand because std's accessor for the file
    // index (`MetadataExt::file_index`) is behind the unstable `windows_by_handle` feature, and
    // pulling the `windows` crate into the dependency-free core for one struct is the wrong trade
    // (the user approved the FFI 2026-09-15). Contained: one call, one `unsafe` block, safe fallback.
    #[repr(C)]
    #[allow(non_snake_case)]
    struct FileTime {
        dwLowDateTime: u32,
        dwHighDateTime: u32,
    }
    #[repr(C)]
    #[allow(non_snake_case)]
    struct ByHandleFileInformation {
        dwFileAttributes: u32,
        ftCreationTime: FileTime,
        ftLastAccessTime: FileTime,
        ftLastWriteTime: FileTime,
        dwVolumeSerialNumber: u32,
        nFileSizeHigh: u32,
        nFileSizeLow: u32,
        nNumberOfLinks: u32,
        nFileIndexHigh: u32,
        nFileIndexLow: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            hFile: *mut core::ffi::c_void,
            lpFileInformation: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let mut info = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
    // SAFETY: `fs` is an open `File`, so its raw handle is a valid file handle for the duration of
    // this call; `info` is a correctly sized and aligned `#[repr(C)]` out-parameter that the call
    // fully initialises when it returns nonzero (and which we read only then); the function neither
    // retains the pointer nor the handle.
    let ok = unsafe { GetFileInformationByHandle(fs.as_raw_handle() as *mut _, info.as_mut_ptr()) };
    if ok != 0 {
        // SAFETY: nonzero return == the struct was written in full.
        let info = unsafe { info.assume_init() };
        Some(FileId(
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    } else {
        Some(FileId(0, m.creation_time()))
    }
}

#[cfg(not(any(unix, windows)))]
fn file_id(_fs: &File, _m: &Metadata) -> Option<FileId> {
    None
}

/// What one bounded read produced: the text decoded so far, whether more remains, and the error
/// that stopped it (if any). Text decoded before a mid-read failure is NOT lost — `position` has
/// already advanced past it (review #4 finding 2).
struct ReadOutcome {
    text: Option<String>,
    more: bool,
    error: Option<io::Error>,
}

/// Incrementally tails one file. Safe to call [`poll`](TailReader::poll) repeatedly on a timer.
pub struct TailReader {
    path: PathBuf,
    position: u64,
    decoder: Option<Decoder>,
    file_seen: bool,
    missing: bool,
    encoding_label: String,
    /// Fingerprint of the file's first [`HEAD_FINGERPRINT_LEN`] bytes (or as many as existed) at
    /// the last (re)start, for in-place-rotation detection (review B9, DECISIONS R102). `None`
    /// until the file is first seen. GROWS with the file until it is full (review #2 finding 1):
    /// a file first seen empty or short used to keep a 0-byte fingerprint forever, which could
    /// never differ from anything, so in-place rotation was undetectable for that file's life.
    head: Option<Vec<u8>>,
    /// The file object's identity at the last (re)start (review #4 finding 3); a different identity
    /// at a length change, a periodic re-check, or a reappearance is a new file ⇒ restart.
    identity: Option<FileId>,
    /// CURSOR fingerprint (review #6 finding 4, DECISIONS R148): the last (up to)
    /// `HEAD_FINGERPRINT_LEN` raw bytes ending exactly at `position`, refreshed by every successful
    /// read. A file whose identity and head are unchanged but whose bytes just before our cursor
    /// differ has been rewritten under us — copy-truncate with a constant banner, or a Linux
    /// delete+recreate that reused the inode — and must restart from the top. Empty right after a
    /// (re)start that landed past the head window (fills on the first read); empty means "cannot
    /// judge", never "changed".
    cursor_fp: Vec<u8>,
    /// Length seen at the previous successful poll — the head fingerprint is re-checked when this
    /// changes (finding 7), plus periodically.
    last_len: u64,
    /// Polls since the head fingerprint was last compared (finding 7).
    polls_since_head_check: u32,
    /// Bound on how much existing history a (re)start loads; see [`INITIAL_LOAD_CAP`]. Tests use a
    /// small value.
    initial_load_cap: u64,
    /// Most bytes decoded per poll; see [`MAX_BYTES_PER_POLL`]. Tests use a small value.
    max_bytes_per_poll: u64,
    /// `ready` is reported on the first successful poll only.
    reported_ready: bool,
    /// Polls spent holding back a 1–3-byte no-BOM first sight (review #4 finding 14).
    short_head_polls: u32,
}

impl TailReader {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        TailReader {
            path: path.as_ref().to_path_buf(),
            position: 0,
            decoder: None,
            file_seen: false,
            missing: false,
            encoding_label: String::new(),
            head: None,
            identity: None,
            cursor_fp: Vec::new(),
            last_len: 0,
            polls_since_head_check: 0,
            initial_load_cap: INITIAL_LOAD_CAP,
            max_bytes_per_poll: MAX_BYTES_PER_POLL,
            reported_ready: false,
            short_head_polls: 0,
        }
    }

    /// Override the initial-history bound and the per-poll read bound (tests; the app uses the
    /// defaults). `0` for either means "unbounded".
    pub fn with_limits(mut self, initial_load_cap: u64, max_bytes_per_poll: u64) -> Self {
        self.initial_load_cap = if initial_load_cap == 0 {
            u64::MAX
        } else {
            initial_load_cap
        };
        self.max_bytes_per_poll = if max_bytes_per_poll == 0 {
            u64::MAX
        } else {
            max_bytes_per_poll
        };
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read whatever is newly available (at most [`MAX_BYTES_PER_POLL`] bytes; `more` says whether
    /// to call again right away).
    pub fn poll(&mut self) -> TailResult {
        // Open shared read. On Windows, File::open requests shared read access by default so it
        // does not lock the writer (mirrors the .NET FileShare.ReadWrite|Delete intent for the
        // read side). A not-found error is the "missing" path.
        let mut fs = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return self.handle_missing(),
            Err(e) => {
                // Sharing/lock/permission error: nothing to read this tick, but SAY SO (review B2)
                // — the caller edge-triggers the message so a locked file shows one marker instead
                // of "loading..." forever.
                return self.error_result(format!("cannot open: {e}"));
            }
        };

        let meta = match fs.metadata() {
            Ok(m) => m,
            Err(e) => return self.error_result(format!("cannot stat: {e}")),
        };
        let len = meta.len();
        let id = file_id(&fs, &meta);

        // Decide (and apply) how this poll positions itself: first sight, reappearance, rotation,
        // or a plain continuation. Every seek/read inside is fallible and reported, never swallowed
        // (review #4 finding 2).
        let (rotated, reappeared, skipped_bytes) = match self.advance(&mut fs, len, id) {
            Ok(Some(t)) => t,
            // Held back: a 1–3-byte no-BOM first sight waits for enough bytes to detect the
            // encoding (review #4 finding 14). Nothing to report yet; not `ready`.
            Ok(None) => {
                return TailResult {
                    exists: true,
                    ..Default::default()
                };
            }
            Err(e) => return self.error_result(format!("cannot read: {e}")),
        };
        self.last_len = len;

        let out = self.read_from(&mut fs, len);
        let ready = !self.reported_ready;
        self.reported_ready = true;

        TailResult {
            new_text: out.text,
            rotated,
            reappeared,
            missing: false,
            exists: true,
            encoding_label: self.encoding_label.clone(),
            error: out.error.map(|e| format!("cannot read: {e}")),
            skipped_bytes,
            more: out.more,
            ready,
        }
    }

    fn handle_missing(&mut self) -> TailResult {
        self.missing = true;
        TailResult {
            missing: true,
            exists: false,
            encoding_label: self.encoding_label.clone(),
            ..Default::default()
        }
    }

    /// A poll that could not open/stat/read: nothing consumed, `error` set, never `more`.
    fn error_result(&self, msg: String) -> TailResult {
        TailResult {
            exists: self.file_seen && !self.missing,
            encoding_label: self.encoding_label.clone(),
            error: Some(msg),
            ..Default::default()
        }
    }

    /// Position the reader for this poll. Returns `Ok(Some((rotated, reappeared, skipped_bytes)))`
    /// when reading may proceed, `Ok(None)` when the poll is held back (short no-BOM first sight),
    /// `Err` on any seek/read failure (state is left as it was, so the next poll retries cleanly).
    fn advance(
        &mut self,
        fs: &mut File,
        len: u64,
        id: Option<FileId>,
    ) -> io::Result<Option<(bool, bool, Option<u64>)>> {
        let mut rotated = false;
        let mut reappeared = false;
        let mut skipped_bytes = None;

        // Every (re)start below may be HELD BACK (`Start::Deferred`, review #4 finding 14) — then
        // nothing is committed or reported this poll and the same branch is re-entered next poll.
        if !self.file_seen {
            match self.start_from_beginning(fs, len, id)? {
                Start::Deferred => return Ok(None),
                Start::Positioned(skipped) => skipped_bytes = skipped,
            }
            self.file_seen = true;
            self.missing = false;
        } else if self.missing {
            // The SAME file coming back (a share blip, a re-mounted drive: same identity, length not
            // shrunk, head fingerprint intact) continues from where we were — no second copy of the
            // whole file (review #2 finding 2). A genuinely recreated file restarts from the top.
            // `position == 0` (nothing consumed yet — the file was EMPTY at first sight) restarts too,
            // so the encoding is re-detected: an empty file recreated by a UTF-16 writer used to
            // keep the stale UTF-8 decoder for life (review #4 finding 4).
            let restart = len < self.position
                || self.position == 0
                || self.identity != id
                || self.head_changed(fs)?
                || self.cursor_changed(fs)?;
            if restart {
                match self.start_from_beginning(fs, len, id)? {
                    Start::Deferred => return Ok(None),
                    Start::Positioned(skipped) => skipped_bytes = skipped,
                }
            }
            self.missing = false;
            reappeared = true;
        } else if len < self.position {
            // Truncation-shrank rotation: the file got SHORTER than where we were reading.
            match self.start_from_beginning(fs, len, id)? {
                Start::Deferred => return Ok(None),
                Start::Positioned(skipped) => skipped_bytes = skipped,
            }
            rotated = true;
        } else if self.head_check_due(len)
            && (self.identity != id || self.head_changed(fs)? || self.cursor_changed(fs)?)
        {
            // A NEW FILE OBJECT at the old path (review #4 finding 3: rename-and-recreate whose
            // header matches), or in-place (copy-truncate) rotation: the file was refilled to the
            // same-or-larger size in one interval, so `len < position` never fired, but the head
            // bytes differ — it's a new file's content at the old path. Or (R148) the same object
            // with the same head but DIFFERENT bytes just before our cursor: a refill behind a
            // constant banner longer than the head window. Re-read from the top (review B9,
            // DECISIONS R102).
            match self.start_from_beginning(fs, len, id)? {
                Start::Deferred => return Ok(None),
                Start::Positioned(skipped) => skipped_bytes = skipped,
            }
            rotated = true;
        } else {
            match self.grow_fingerprint(fs, len, id)? {
                Start::Deferred => return Ok(None),
                Start::Positioned(skipped) => skipped_bytes = skipped,
            }
        }
        Ok(Some((rotated, reappeared, skipped_bytes)))
    }

    /// Whether this poll should compare the head fingerprint / identity (finding 7): when the
    /// length changed since the last poll, or every [`HEAD_RECHECK_POLLS`] polls regardless.
    fn head_check_due(&mut self, len: u64) -> bool {
        self.polls_since_head_check = self.polls_since_head_check.saturating_add(1);
        if len != self.last_len || self.polls_since_head_check >= HEAD_RECHECK_POLLS {
            self.polls_since_head_check = 0;
            true
        } else {
            false
        }
    }

    /// Detect encoding from the head and position the cursor just past any BOM. Also captures the
    /// head fingerprint for in-place-rotation detection (review B9, DECISIONS R102) and the file
    /// identity (review #4 finding 3). If the file is longer than the initial-history bound, the
    /// cursor is moved to the first line boundary after `len - cap` instead and the skipped byte
    /// count is returned (review #2 finding 5). Commits its state only after every read succeeded.
    fn start_from_beginning(
        &mut self,
        fs: &mut File,
        len: u64,
        id: Option<FileId>,
    ) -> io::Result<Start> {
        // Read the fingerprint window in one seek+read.
        let mut head = [0u8; HEAD_FINGERPRINT_LEN];
        fs.seek(SeekFrom::Start(0))?;
        let hn = read_up_to(fs, &mut head)?;
        // Detection sees the WHOLE head window (review #4 finding 14) — it used to get at most 4
        // bytes, which is exactly the heuristic's minimum and nothing more.
        let det = encoding::detect(&head[..hn], hn);
        // A 1–3-byte first sight with no BOM cannot be classified yet (the UTF-16 heuristic needs
        // ≥ 4 bytes): if those bytes could be UTF-16 — a NUL among them, which UTF-16 Latin text
        // always has and UTF-8 text never does — hold back a few polls for the writer's next flush
        // rather than committing UTF-8 for the file's life (finding 14). Plain bytes (`x\n`) commit
        // at once, so a tiny hand-made file never waits. An EMPTY file is not held back — it renders
        // blank (review #2 finding 4) and re-detects on its first bytes via `grow_fingerprint`.
        if (1..4).contains(&hn)
            && det.bom_length == 0
            && head[..hn].contains(&0)
            && self.short_head_polls < SHORT_HEAD_DEFER_POLLS
        {
            self.short_head_polls += 1;
            return Ok(Start::Deferred);
        }

        let mut position = det.bom_length as u64;
        let cap = self.initial_load_cap;
        let skipped = if len > cap && len - cap > position {
            let from = len - cap;
            let start = align_after_newline(fs, from, len, det.encoding);
            position = start;
            Some(start)
        } else {
            None
        };

        // Commit.
        self.decoder = Some(det.encoding.new_decoder_without_bom_handling());
        self.position = position;
        self.encoding_label = det.label.to_string();
        self.head = Some(head[..hn].to_vec());
        // The cursor fingerprint is whatever of the head window sits before the new position (the
        // BOM, typically); a start past the window leaves it empty until the first read (R148).
        self.cursor_fp = if position as usize <= hn {
            head[..position as usize].to_vec()
        } else {
            Vec::new()
        };
        self.identity = id;
        self.polls_since_head_check = 0;
        self.short_head_polls = 0;
        Ok(Start::Positioned(skipped))
    }

    /// Whether the file's head bytes differ from the fingerprint captured at the last (re)start —
    /// the signature of an in-place (copy-truncate) rotation (review B9, DECISIONS R102). Reads the
    /// head window fresh (leaving the cursor moved; the caller either restarts or seeks in
    /// `read_from`). A seek/read failure is an `Err` for the caller to report (review #4 findings
    /// 2 and 12) — never a false "rotated", never a silent "same".
    fn head_changed(&mut self, fs: &mut File) -> io::Result<bool> {
        let prev = match self.head.as_ref() {
            Some(h) => h,
            None => return Ok(false),
        };
        let mut cur = [0u8; HEAD_FINGERPRINT_LEN];
        fs.seek(SeekFrom::Start(0))?;
        let n = read_up_to(fs, &mut cur)?;
        // Compare only as many bytes as the old fingerprint held: appends past the head never move
        // these bytes, so an equal prefix means "same file, just grew".
        Ok(prev.as_slice() != &cur[..n.min(prev.len())] || n < prev.len())
    }

    /// Whether the bytes just before the cursor differ from the cursor fingerprint captured by the
    /// last read (review #6 finding 4, DECISIONS R148) — the signature of a rewrite that kept the
    /// identity and the head. Costs one seek + one read of at most `HEAD_FINGERPRINT_LEN` bytes,
    /// and only when the caller is already re-checking (a length change, the periodic re-check, a
    /// reappearance). An empty fingerprint cannot judge and reports "same". A read that comes up
    /// short (the file is shorter than the cursor after all) reports "changed" so the caller
    /// restarts rather than reading past the end.
    fn cursor_changed(&mut self, fs: &mut File) -> io::Result<bool> {
        let n = self.cursor_fp.len();
        if n == 0 || (n as u64) > self.position {
            return Ok(false);
        }
        let mut cur = [0u8; HEAD_FINGERPRINT_LEN];
        fs.seek(SeekFrom::Start(self.position - n as u64))?;
        let got = read_up_to(fs, &mut cur[..n])?;
        Ok(got < n || cur[..n] != self.cursor_fp[..])
    }

    /// Extend a SHORT fingerprint as the file grows (review #2 finding 1): a file first seen empty
    /// or shorter than the window kept a 0..63-byte fingerprint forever, so later in-place
    /// rotations were undetectable. If nothing has been decoded yet (position still 0 — the file
    /// was EMPTY at first sight) the encoding is re-detected too, so a BOM written after the window
    /// opened is honoured instead of rendering as `��`.
    fn grow_fingerprint(
        &mut self,
        fs: &mut File,
        len: u64,
        id: Option<FileId>,
    ) -> io::Result<Start> {
        let short = match self.head.as_ref() {
            Some(h) => h.len() < HEAD_FINGERPRINT_LEN && (h.len() as u64) < len,
            None => false,
        };
        if !short {
            return Ok(Start::Positioned(None));
        }
        if self.position == 0 && self.head.as_ref().is_some_and(|h| h.len() < 4) {
            // Nothing consumed yet: a clean restart re-detects the encoding and re-fingerprints
            // (and applies the initial-history bound, should the file have jumped past it). May
            // be held back like any start (finding 14) — e.g. an empty first sight that now holds
            // 2 bytes of a no-BOM UTF-16 writer's first flush.
            return self.start_from_beginning(fs, len, id);
        }
        let mut cur = [0u8; HEAD_FINGERPRINT_LEN];
        fs.seek(SeekFrom::Start(0))?;
        let n = read_up_to(fs, &mut cur)?;
        let prev = self.head.as_ref().map(|h| h.len()).unwrap_or(0);
        if n > prev && self.head.as_deref() == Some(&cur[..prev]) {
            self.head = Some(cur[..n].to_vec());
        }
        Ok(Start::Positioned(None))
    }

    /// Decode from `position` up to `len`, but at most `max_bytes_per_poll` bytes; returns the text
    /// decoded, whether bytes remain (`more` — only when this call actually advanced and no error
    /// stopped it), and the error that stopped it, if any.
    fn read_from(&mut self, fs: &mut File, len: u64) -> ReadOutcome {
        let none = ReadOutcome {
            text: None,
            more: false,
            error: None,
        };
        if self.decoder.is_none() || len <= self.position {
            return none;
        }
        if let Err(e) = fs.seek(SeekFrom::Start(self.position)) {
            return ReadOutcome {
                error: Some(e),
                ..none
            };
        }

        let decoder = self.decoder.as_mut().unwrap();
        let start_position = self.position;
        let mut remaining = (len - self.position).min(self.max_bytes_per_poll);
        let mut out = String::new();
        let mut buf = [0u8; CHUNK_SIZE];
        let mut error = None;

        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = match fs.read(&mut buf[..want]) {
                Ok(0) => break, // EOF before `len` (the file shrank under us): next poll re-stats
                Ok(n) => n,
                Err(e) => {
                    // Report it (review #4 finding 2); what was decoded so far is kept — `position`
                    // already covers it.
                    error = Some(e);
                    break;
                }
            };
            // Feed ALL n bytes to the decoder with last=false — even when this chunk yields zero
            // complete chars — so an incomplete trailing multi-byte sequence is BUFFERED in the
            // decoder's internal state for the next poll instead of being dropped. THIS is the
            // multibyte-split invariant.
            //
            // The output String MUST be pre-sized to max_utf8_buffer_length: with too little
            // capacity, decode returns OutputFull having consumed ZERO input bytes, and we would
            // then advance `position` past bytes the decoder never saw — silently losing the split
            // char. (That was the first-cut bug this test caught.) We loop on the input slice until
            // every byte is consumed, so `position` always advances exactly by bytes decoded.
            // REPLACEMENT decoding (review B8, DECISIONS R97): an undecodable byte (a Windows-1252
            // `é` in a file detected as UTF-8, say) becomes U+FFFD — visible, like the .NET
            // `Decoder` the spec was ported from. The previous `_without_replacement` variant
            // silently DROPPED such bytes, so a legacy-codepage log lost characters with no trace.
            let mut input = &buf[..n];
            loop {
                let cap = decoder
                    .max_utf8_buffer_length(input.len())
                    .unwrap_or(input.len() * 4 + 16)
                    .max(16);
                let mut chunk_out = String::with_capacity(cap);
                let (res, read, _had_replacements) =
                    decoder.decode_to_string(input, &mut chunk_out, false);
                out.push_str(&chunk_out);
                input = &input[read..];
                match res {
                    // All input consumed (possibly buffering a partial trailing char) — done.
                    CoderResult::InputEmpty => break,
                    // Need more output room; loop with a fresh buffer for the remaining input.
                    // GUARD: `chunk_out` is pre-sized to `max_utf8_buffer_length`, so a well-behaved
                    // decoder always consumes ≥1 byte before reporting OutputFull. If it ever
                    // reports OutputFull having consumed ZERO bytes (a sizing shortfall from the
                    // fallback capacity, or a future encoding change), looping again with the same
                    // input would spin forever — position never advances. Break instead: a hang is
                    // never acceptable; dropping a few undecodable trailing bytes in that
                    // should-be-impossible case is the safe failure.
                    CoderResult::OutputFull => {
                        if input.is_empty() || read == 0 {
                            break;
                        }
                    }
                }
            }
            self.position += n as u64;
            remaining -= n as u64;
            // Keep the cursor fingerprint = the last HEAD_FINGERPRINT_LEN raw bytes read (R148).
            if n >= HEAD_FINGERPRINT_LEN {
                self.cursor_fp.clear();
                self.cursor_fp
                    .extend_from_slice(&buf[n - HEAD_FINGERPRINT_LEN..n]);
            } else {
                self.cursor_fp.extend_from_slice(&buf[..n]);
                if self.cursor_fp.len() > HEAD_FINGERPRINT_LEN {
                    let drop = self.cursor_fp.len() - HEAD_FINGERPRINT_LEN;
                    self.cursor_fp.drain(..drop);
                }
            }
        }

        // `more` only when this call made progress AND nothing went wrong (review #4 finding 2):
        // a caller that loops on it can then never spin on a file it cannot read.
        let progressed = self.position > start_position;
        let more = error.is_none() && progressed && self.position < len;
        let text = if out.is_empty() { None } else { Some(out) };
        ReadOutcome { text, more, error }
    }
}

/// How a (re)start left the reader: positioned (with the skipped-bytes count, if any), or held back.
enum Start {
    Positioned(Option<u64>),
    Deferred,
}

/// Read as much as fits into `buf` in one or more reads (up to buf.len()), returning byte count;
/// an I/O error is returned, not swallowed (review #4 findings 2 and 12).
fn read_up_to(fs: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match fs.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// The first offset at or after `from` that begins a line — i.e. just past the next line
/// terminator — for the detected encoding (UTF-8/ASCII: a `\n` byte; UTF-16: a `\n` code unit on
/// a 2-byte boundary). Falls back to `from` itself if no terminator is found before `len` or a
/// read fails (one enormous line: the decoder's replacement handling absorbs a mid-character
/// start; a read failure is reported by the read that follows). A terminator whose following
/// offset is `len` (the file's only newline in the last `cap` bytes is its final byte, e.g. a
/// single over-cap line ending in `\n`) is treated as "not found": returning `len` would leave the
/// reader with nothing to read and the whole file skipped (REVIEW-2026-09-17 finding 3), so we fall
/// back to `from` and let the decoder's replacement handling absorb the mid-line start instead.
fn align_after_newline(
    fs: &mut File,
    from: u64,
    len: u64,
    enc: &'static encoding_rs::Encoding,
) -> u64 {
    let unit: u64 = if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
        2
    } else {
        1
    };
    // Keep UTF-16 code units aligned (the BOM, if any, is 2 bytes, so parity is preserved).
    let from = from - (from % unit);
    if fs.seek(SeekFrom::Start(from)).is_err() {
        return from;
    }
    let mut buf = [0u8; CHUNK_SIZE];
    let mut pos = from;
    while pos < len {
        let n = match read_up_to(fs, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let n = n - (n % unit as usize);
        let mut i = 0;
        while i + unit as usize <= n {
            let is_nl = if unit == 1 {
                buf[i] == b'\n'
            } else if enc == encoding_rs::UTF_16LE {
                buf[i] == b'\n' && buf[i + 1] == 0
            } else {
                buf[i] == 0 && buf[i + 1] == b'\n'
            };
            if is_nl {
                let boundary = pos + i as u64 + unit;
                // A boundary at EOF is not a usable line start: it would position the reader at
                // `len`, so it reads nothing and the entire file is reported as skipped
                // (finding 3). Fall through to the `from` fallback in that case.
                if boundary < len {
                    return boundary;
                }
            }
            i += unit as usize;
        }
        pos += n as u64;
        if n == 0 {
            break;
        }
    }
    from
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmpdir() -> PathBuf {
        crate::test_support::tmpdir("dwtail")
    }

    fn write_utf8(path: &Path, s: &str) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    fn append_bytes(path: &Path, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    // Ported from TailReaderTests.cs (parity oracle).

    #[test]
    fn reads_initial_then_appends_then_nothing() {
        let d = tmpdir();
        let f = d.join("a.log");
        write_utf8(&f, "line1\n");
        let mut tr = TailReader::new(&f);
        assert_eq!(tr.poll().new_text.as_deref(), Some("line1\n"));

        append_bytes(&f, b"line2\n");
        assert_eq!(tr.poll().new_text.as_deref(), Some("line2\n"));

        assert_eq!(tr.poll().new_text, None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rotation_detected_on_truncate() {
        let d = tmpdir();
        let f = d.join("b.log");
        write_utf8(&f, "aaaa\nbbbb\n");
        let mut tr = TailReader::new(&f);
        tr.poll();

        write_utf8(&f, "new\n"); // shorter -> truncation
        let res = tr.poll();
        assert!(res.rotated);
        assert_eq!(res.new_text.as_deref(), Some("new\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn in_place_rotation_same_or_larger_size_is_detected_by_head_change() {
        // B9 (DECISIONS R102): copy-truncate rotation refills the file to the SAME-OR-LARGER size in
        // one interval, so `len < position` never fires. The old code (truncation-shrank only) would
        // append the new file's content onto the old buffer as if continuous. The head fingerprint
        // catches it. This test FAILS on .i29 (no `rotated`, wrong `new_text`) and passes now.
        let d = tmpdir();
        let f = d.join("c.log");
        write_utf8(&f, "OLD-A\nOLD-B\n"); // 12 bytes
        let mut tr = TailReader::new(&f);
        assert_eq!(tr.poll().new_text.as_deref(), Some("OLD-A\nOLD-B\n"));

        // Rotate IN PLACE to a same-or-larger file with different head bytes (no length shrink):
        write_utf8(&f, "NEW-1\nNEW-2\nNEW-3\n"); // 18 bytes > 12
        let res = tr.poll();
        assert!(
            res.rotated,
            "an in-place rotation with changed head bytes must be flagged rotated"
        );
        assert_eq!(
            res.new_text.as_deref(),
            Some("NEW-1\nNEW-2\nNEW-3\n"),
            "after an in-place rotation the tail re-reads from the top, not from the old position"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_plain_append_is_not_mistaken_for_an_in_place_rotation() {
        // Guard the B9 detector against false positives: a normal append leaves the head intact, so
        // `rotated` must stay false and only the appended bytes come back (DECISIONS R102).
        let d = tmpdir();
        let f = d.join("d.log");
        // Head shorter than the fingerprint window, then grown well past it: the intact prefix must
        // still read as "same file".
        write_utf8(&f, "hdr\n");
        let mut tr = TailReader::new(&f);
        assert_eq!(tr.poll().new_text.as_deref(), Some("hdr\n"));

        let big = "data data data\n".repeat(20);
        append_bytes(&f, big.as_bytes());
        let res = tr.poll();
        assert!(
            !res.rotated,
            "a plain append must not be flagged as a rotation"
        );
        assert_eq!(res.new_text.as_deref(), Some(big.as_str()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn deletion_then_reappearance() {
        let d = tmpdir();
        let f = d.join("c.log");
        write_utf8(&f, "one\n");
        let mut tr = TailReader::new(&f);
        tr.poll();

        fs::remove_file(&f).unwrap();
        assert!(tr.poll().missing);

        write_utf8(&f, "two\n");
        let res = tr.poll();
        assert!(res.reappeared);
        assert_eq!(res.new_text.as_deref(), Some("two\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn utf16le_with_bom_decoded() {
        let d = tmpdir();
        let f = d.join("d.log");
        // UTF-16 LE with BOM: FF FE, then "héllo\n".
        let mut bytes = vec![0xFF, 0xFE];
        for u in "héllo\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        fs::write(&f, &bytes).unwrap();
        let mut tr = TailReader::new(&f);
        let res = tr.poll();
        assert_eq!(res.new_text.as_deref(), Some("héllo\n"));
        assert!(res.encoding_label.contains("UTF-16"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn multibyte_char_split_across_polls() {
        // "é" = 0xC3 0xA9 in UTF-8. Write the two bytes in separate appends.
        let d = tmpdir();
        let f = d.join("e.log");
        write_utf8(&f, "");
        let mut tr = TailReader::new(&f);
        tr.poll(); // empty

        append_bytes(&f, &[0xC3]); // 1 byte, no NUL: committed at once (finding 14 rule)
        let mid = tr.poll(); // partial multi-byte: decoder buffers, no char yet
        assert!(mid.new_text.as_deref().unwrap_or("").is_empty());

        append_bytes(&f, &[0xA9, b'\n']);
        let done = tr.poll();
        assert_eq!(done.new_text.as_deref(), Some("é\n"));
        let _ = fs::remove_dir_all(&d);
    }

    // --- .i29 (review B2 / B8, DECISIONS R97) ---

    #[test]
    fn undecodable_byte_becomes_replacement_char_not_dropped() {
        // review B8: a Windows-1252 `é` (0xE9) in a file detected as UTF-8 must show as U+FFFD, the
        // .NET behavior — not vanish. Before R97 "caf\xE9 au lait" rendered as "caf au lait".
        let d = tmpdir();
        let f = d.join("cp1252.log");
        fs::write(&f, b"caf\xE9 au lait\n").unwrap();
        let mut tr = TailReader::new(&f);
        let res = tr.poll();
        assert_eq!(res.new_text.as_deref(), Some("caf\u{FFFD} au lait\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_file_reports_an_error_not_silence() {
        // review B2: permission denied used to look exactly like "nothing new this poll".
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir();
        let f = d.join("locked.log");
        fs::write(&f, "secret\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything, so under uid 0 this test cannot test (review #2 finding 13):
        // say so LOUDLY instead of passing vacuously. Everywhere else the assertions are
        // unconditional.
        if fs::File::open(&f).is_ok() {
            eprintln!(
                "SKIPPED unreadable_file_reports_an_error_not_silence: running as root, \
                 mode 000 does not refuse reads here — proved on a non-root host"
            );
            fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
            let _ = fs::remove_dir_all(&d);
            return;
        }
        let mut tr = TailReader::new(&f);
        let res = tr.poll();
        let err = res.error.expect("an unreadable file must report an error");
        assert!(err.starts_with("cannot open:"), "{err}");
        assert!(!res.missing);
        assert!(res.new_text.is_none());
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    #[cfg(windows)]
    fn sharing_violation_reports_an_error_not_silence() {
        // review B2 on the platform it was raised for (review #2 finding 13): a writer holding the
        // file with NO share access (share_mode 0) makes every open a sharing violation. This runs
        // on the user's Windows gate, where there is no root bypass.
        use std::os::windows::fs::OpenOptionsExt;
        let d = tmpdir();
        let f = d.join("locked.log");
        fs::write(&f, "secret\n").unwrap();
        let _holder = fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&f)
            .unwrap();
        let mut tr = TailReader::new(&f);
        let res = tr.poll();
        let err = res.error.expect("a sharing violation must report an error");
        assert!(err.starts_with("cannot open:"), "{err}");
        assert!(!res.missing);
        assert!(res.new_text.is_none());
        drop(_holder);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn normal_polls_carry_no_error() {
        let d = tmpdir();
        let f = d.join("ok.log");
        write_utf8(&f, "fine\n");
        let mut tr = TailReader::new(&f);
        assert!(tr.poll().error.is_none());
        assert!(tr.poll().error.is_none());
        let _ = fs::remove_dir_all(&d);
    }
    // --- .i32 (review #2 findings 1, 2, 4, 5, 7; DECISIONS R110-R114) ---

    #[test]
    fn empty_at_first_sight_still_detects_a_later_in_place_rotation() {
        // REGRESSION (review #2 finding 1): a 0-byte fingerprint could never differ from anything,
        // so a file first polled while EMPTY (the auto-open-on-creation case) never detected an
        // in-place rotation for its whole life. On .i31: rotated=false, new_text="NEW-3\n".
        let d = tmpdir();
        let f = d.join("c.log");
        write_utf8(&f, "");
        let mut tr = TailReader::new(&f);
        assert_eq!(tr.poll().new_text, None);
        append_bytes(&f, b"OLD-A\nOLD-B\n");
        assert_eq!(tr.poll().new_text.as_deref(), Some("OLD-A\nOLD-B\n"));
        write_utf8(&f, "NEW-1\nNEW-2\nNEW-3\n"); // in place, larger, different head
        let r = tr.poll();
        assert!(
            r.rotated,
            "in-place rotation must be detected after an empty first sight"
        );
        assert_eq!(r.new_text.as_deref(), Some("NEW-1\nNEW-2\nNEW-3\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn short_head_at_first_sight_grows_the_fingerprint() {
        // REGRESSION (finding 1, variant): a 4-byte fingerprint only ever compared 4 bytes, so a
        // new file sharing those 4 bytes was missed. The fingerprint now grows with the file.
        let d = tmpdir();
        let f = d.join("c.log");
        write_utf8(&f, "hdr\n");
        let mut tr = TailReader::new(&f);
        tr.poll();
        append_bytes(&f, b"OLD-A\nOLD-B\n");
        tr.poll(); // fingerprint grows here
        write_utf8(&f, "hdr\nNEW-1\nNEW-2\nNEW-3\n"); // same 4-byte prefix, new file, larger
        let r = tr.poll();
        assert!(r.rotated);
        assert_eq!(r.new_text.as_deref(), Some("hdr\nNEW-1\nNEW-2\nNEW-3\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_at_first_sight_re_detects_encoding_when_a_bom_arrives() {
        // REGRESSION (finding 1, encoding): an empty file committed a UTF-8 decoder forever, so a
        // UTF-16 LE BOM written afterwards (PowerShell 5.1 `>`) rendered as "��h\0i\0". Nothing
        // has been consumed at that point, so the reader re-detects.
        let d = tmpdir();
        let f = d.join("u.log");
        write_utf8(&f, "");
        let mut tr = TailReader::new(&f);
        let first = tr.poll();
        assert!(
            first.ready,
            "the first successful poll reports ready even for an empty file"
        );
        let mut bytes = vec![0xFF, 0xFE];
        for u in "hi\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        append_bytes(&f, &bytes);
        let r = tr.poll();
        assert!(r.encoding_label.contains("UTF-16"), "{}", r.encoding_label);
        assert_eq!(r.new_text.as_deref(), Some("hi\n"));
        assert!(!r.ready, "ready is reported once");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn reappearance_of_the_same_unchanged_file_continues_without_rereading() {
        // REGRESSION (review #2 finding 2): a share blip made the file "missing" for a poll; on
        // return the reader restarted from offset 0 and appended a second copy of everything. The
        // same file (length not shrunk, head intact) now continues from where it was.
        let d = tmpdir();
        let f = d.join("s.log");
        write_utf8(&f, "one\ntwo\n");
        let mut tr = TailReader::new(&f);
        assert_eq!(tr.poll().new_text.as_deref(), Some("one\ntwo\n"));
        let away = d.join("s.away");
        fs::rename(&f, &away).unwrap();
        assert!(tr.poll().missing);
        fs::rename(&away, &f).unwrap();
        let r = tr.poll();
        assert!(r.reappeared);
        assert_eq!(
            r.new_text, None,
            "unchanged file must not be re-read: {:?}",
            r.new_text
        );
        append_bytes(&f, b"three\n");
        assert_eq!(tr.poll().new_text.as_deref(), Some("three\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn reappearance_of_a_recreated_file_restarts_from_the_top() {
        // The finding-2 rule must not break the genuine delete-and-recreate case (R81 behaviour).
        let d = tmpdir();
        let f = d.join("r.log");
        write_utf8(&f, "old-one\nold-two\n");
        let mut tr = TailReader::new(&f);
        tr.poll();
        fs::remove_file(&f).unwrap();
        assert!(tr.poll().missing);
        write_utf8(&f, "NEW-one\nNEW-two\nNEW-three\n"); // longer, different head
        let r = tr.poll();
        assert!(r.reappeared);
        assert_eq!(r.new_text.as_deref(), Some("NEW-one\nNEW-two\nNEW-three\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn initial_load_is_bounded_and_starts_on_a_line_boundary() {
        // REGRESSION (review #2 finding 5): the whole existing file used to be read+decoded into
        // one String (300 MB -> 2.2 s and ~700 MB peak in the app) to keep 48 MB. With a small cap:
        // only the tail of the history loads, from the first line boundary after `len - cap`, and
        // the skipped byte count is reported once.
        let d = tmpdir();
        let f = d.join("big.log");
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("line-{i:03}\n")); // 9 bytes each = 900 bytes
        }
        write_utf8(&f, &content);
        let mut tr = TailReader::new(&f).with_limits(100, 0); // keep ~the last 100 bytes
        let r = tr.poll();
        let skipped = r
            .skipped_bytes
            .expect("skipped_bytes reported on the bounded first load");
        assert!(
            skipped >= 800 && skipped.is_multiple_of(9),
            "skip lands on a line start: {skipped}"
        );
        let text = r.new_text.unwrap();
        assert!(
            text.starts_with("line-"),
            "starts on a line boundary: {text:?}"
        );
        assert!(text.ends_with("line-099\n"));
        assert!(text.len() <= 100);
        assert!(!r.more);
        // Follows normally afterwards, with no second skip.
        append_bytes(&f, b"line-100\n");
        let r2 = tr.poll();
        assert_eq!(r2.new_text.as_deref(), Some("line-100\n"));
        assert_eq!(r2.skipped_bytes, None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn terminated_over_cap_single_line_delivers_its_tail_not_nothing() {
        // REGRESSION (REVIEW-2026-09-17 finding 3): a single line longer than the cap that ends in
        // a newline used to deliver 0 bytes and skip the whole file. `align_after_newline` found the
        // only `\n` at EOF and returned `len`, so the reader sat at `len` with nothing to read and
        // the tail was one skip marker over a blank window. The unterminated variant already worked;
        // this is the terminated one. It must now behave like the unterminated case: skip to the
        // fallback `from` and deliver the last `cap` bytes.
        let d = tmpdir();
        let f = d.join("big-oneline-nl.log");
        let mut content = vec![b'x'; 1000];
        content.push(b'\n');
        fs::write(&f, &content).unwrap();
        let mut tr = TailReader::new(&f).with_limits(100, 0);
        let r = tr.poll();
        let text = r
            .new_text
            .expect("a terminated over-cap line must still deliver its tail, not nothing");
        assert!(
            !text.is_empty() && text.len() <= 100,
            "delivers ~the last cap bytes, got {} bytes",
            text.len()
        );
        assert!(
            text.chars().all(|c| c == 'x' || c == '\n'),
            "delivers the line's own bytes: {text:?}"
        );
        // Something was skipped (the file is over cap), but not the whole file.
        let skipped = r.skipped_bytes.expect("over-cap load reports a skip");
        assert!(
            skipped < content.len() as u64,
            "must not skip the entire file: skipped {skipped} of {}",
            content.len()
        );
        // A later append follows normally, with no second skip.
        append_bytes(&f, b"second\n");
        let r2 = tr.poll();
        assert_eq!(r2.new_text.as_deref(), Some("second\n"));
        assert_eq!(r2.skipped_bytes, None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn initial_load_under_the_cap_is_complete_and_unskipped() {
        let d = tmpdir();
        let f = d.join("small.log");
        write_utf8(&f, "a\nb\nc\n");
        let mut tr = TailReader::new(&f).with_limits(100, 0);
        let r = tr.poll();
        assert_eq!(r.skipped_bytes, None);
        assert_eq!(r.new_text.as_deref(), Some("a\nb\nc\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn bounded_initial_load_of_utf16_stays_on_code_unit_and_line_boundaries() {
        let d = tmpdir();
        let f = d.join("u16.log");
        let mut bytes = vec![0xFF, 0xFE];
        for i in 0..50 {
            for u in format!("row{i:02}\n").encode_utf16() {
                bytes.extend_from_slice(&u.to_le_bytes());
            }
        }
        fs::write(&f, &bytes).unwrap();
        let mut tr = TailReader::new(&f).with_limits(60, 0);
        let r = tr.poll();
        assert!(r.encoding_label.contains("UTF-16 LE"));
        let text = r.new_text.unwrap();
        assert!(text.starts_with("row"), "{text:?}");
        assert!(text.ends_with("row49\n"));
        assert!(r.skipped_bytes.is_some());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_large_history_streams_in_bounded_pieces_with_more_set() {
        // REGRESSION (finding 5, streaming): one poll decodes at most `max_bytes_per_poll`; the
        // rest arrives on immediate re-polls flagged `more`, so a caller can stop in between.
        let d = tmpdir();
        let f = d.join("stream.log");
        let content = "0123456789\n".repeat(10); // 110 bytes
        write_utf8(&f, &content);
        let mut tr = TailReader::new(&f).with_limits(0, 40);
        let mut got = String::new();
        let mut polls = 0;
        loop {
            let r = tr.poll();
            polls += 1;
            assert!(!r.rotated && !r.reappeared);
            if let Some(t) = r.new_text {
                got.push_str(&t);
            }
            if !r.more {
                break;
            }
        }
        assert_eq!(got, content, "pieces concatenate to the whole file");
        assert_eq!(polls, 3, "110 bytes at 40/poll = 3 polls");
        assert_eq!(tr.poll().new_text, None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn multibyte_char_split_across_streamed_pieces_survives() {
        // The piece boundary is arithmetic, so it can fall inside a multi-byte char; the stateful
        // decoder must carry it over exactly as it does across polls.
        let d = tmpdir();
        let f = d.join("split.log");
        write_utf8(&f, "ab\u{e9}cd\n"); // "é" = C3 A9 at bytes 2..4
        let mut tr = TailReader::new(&f).with_limits(0, 3); // boundary at byte 3, mid-"é"
        let mut got = String::new();
        loop {
            let r = tr.poll();
            if let Some(t) = r.new_text {
                got.push_str(&t);
            }
            if !r.more {
                break;
            }
        }
        assert_eq!(got, "ab\u{e9}cd\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn same_size_in_place_rotation_is_still_caught_by_the_periodic_head_check() {
        // finding 7: the head is compared only when the length changes, plus every
        // HEAD_RECHECK_POLLS polls — so an in-place rewrite to EXACTLY the same size is caught
        // within that many polls, not never.
        let d = tmpdir();
        let f = d.join("same.log");
        write_utf8(&f, "AAAA\nBBBB\n");
        let mut tr = TailReader::new(&f);
        tr.poll();
        write_utf8(&f, "CCCC\nDDDD\n"); // same length, different bytes
        let mut rotated = false;
        for _ in 0..(HEAD_RECHECK_POLLS + 1) {
            let r = tr.poll();
            if r.rotated {
                rotated = true;
                assert_eq!(r.new_text.as_deref(), Some("CCCC\nDDDD\n"));
                break;
            }
        }
        assert!(
            rotated,
            "same-size rewrite must be detected within the periodic re-check"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ready_is_reported_on_the_first_present_poll_only() {
        let d = tmpdir();
        let f = d.join("ready.log");
        write_utf8(&f, "x\n");
        let mut tr = TailReader::new(&f);
        assert!(tr.poll().ready);
        assert!(!tr.poll().ready);
        let _ = fs::remove_dir_all(&d);
    }

    // --- .i40 (review #4 findings 2, 3, 4, 12, 14; DECISIONS R128) ---

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if bom {
            v.extend_from_slice(&[0xFF, 0xFE]);
        }
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn empty_first_sight_then_recreated_as_utf16_re_detects_on_reappearance() {
        // REGRESSION (review #4 finding 4): a file first seen EMPTY, deleted, then recreated by a
        // UTF-16 writer (PowerShell 5.1 `>`) kept the stale UTF-8 decoder — on .i39 the reappear
        // poll read `label="UTF-8/ASCII" text="��h\0i\0\n\0"`. `position == 0` now restarts.
        let d = tmpdir();
        let f = d.join("u.log");
        write_utf8(&f, "");
        let mut tr = TailReader::new(&f);
        assert!(tr.poll().ready);
        fs::remove_file(&f).unwrap();
        assert!(tr.poll().missing);
        fs::write(&f, utf16le("hi\n", true)).unwrap();
        let r = tr.poll();
        assert!(r.reappeared);
        assert_eq!(r.new_text.as_deref(), Some("hi\n"));
        assert!(r.encoding_label.contains("UTF-16"), "{}", r.encoding_label);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn recreated_file_with_an_identical_64_byte_header_is_detected_by_identity() {
        // REGRESSION (review #4 finding 3): a rename-and-recreate whose new file shares the old
        // file's first 64 bytes (a fixed startup banner) and is already longer than `position`
        // used to continue mid-file — on .i39: `reappeared=true rotated=false text="NEW-3\nNEW-4\n"`,
        // NEW-1/2 never shown. The file identity now forces a restart: (dev, inode) on Unix; on
        // Windows the MFT file index via `GetFileInformationByHandle` (R129 — `.i40`'s creation time
        // was defeated by NTFS tunneling on exactly this rename-then-recreate-immediately sequence,
        // the user's item 10). Runs on EVERY platform; on the user's Windows gate it is the FFI's proof.
        let d = tmpdir();
        let f = d.join("banner.log");
        let header = "#Software: Example Server 10.0 build 12345 -- constant banner line\n";
        assert!(header.len() >= 64);
        write_utf8(&f, &format!("{header}old-1\nold-2\n"));
        let mut tr = TailReader::new(&f);
        assert!(tr.poll().new_text.is_some());
        // Rotate: old file renamed away, a NEW file with the same header and MORE content created.
        fs::rename(&f, d.join("banner.log.1")).unwrap();
        write_utf8(&f, &format!("{header}NEW-1\nNEW-2\nNEW-3\nNEW-4\n"));
        let r = tr.poll();
        assert!(
            r.rotated || r.reappeared,
            "a new file object must restart the tail"
        );
        assert_eq!(
            r.new_text.as_deref(),
            Some(format!("{header}NEW-1\nNEW-2\nNEW-3\nNEW-4\n").as_str()),
            "the NEW file is read from its top, not from the old offset"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    #[cfg(windows)]
    fn windows_file_identity_changes_on_an_immediate_same_name_recreate() {
        // R129: the identity must be the MFT file index, not the creation time — NTFS tunneling
        // gives a file recreated under the same name within ~15 s of the rename the OLD creation
        // time (the .i40 item-10 FAIL). Rename and recreate back-to-back, as loggers do.
        let d = tmpdir();
        let f = d.join("id.log");
        write_utf8(&f, "one\n");
        let a = {
            let fs = fs::File::open(&f).unwrap();
            let m = fs.metadata().unwrap();
            file_id(&fs, &m).unwrap()
        };
        fs::rename(&f, d.join("id.old")).unwrap();
        write_utf8(&f, "one\n"); // same name, same bytes, immediately
        let b = {
            let fs = fs::File::open(&f).unwrap();
            let m = fs.metadata().unwrap();
            file_id(&fs, &m).unwrap()
        };
        assert_ne!(
            a, b,
            "a recreated file must have a new file index even under tunneling"
        );
        assert_ne!(
            a.0, 0,
            "the volume serial is populated (the FFI call succeeded, not the fallback)"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    #[cfg(unix)]
    fn read_error_is_reported_and_never_sets_more() {
        // REGRESSION (review #4 finding 2): a path whose open+stat succeed but whose read fails (a
        // directory, on Unix; a byte-range-locked region or a cloud placeholder on Windows) used to
        // return `more=true, error=None` — the runtime looped on `more` with no sleep: a 100 % CPU
        // spin with nothing shown. On .i39: `1000 polls: more=1000 errors=0`.
        let d = tmpdir();
        let sub = d.join("dir.log");
        fs::create_dir_all(&sub).unwrap();
        let mut tr = TailReader::new(&sub).with_limits(0, 100);
        let mut mores = 0;
        let mut errs = 0;
        for _ in 0..20 {
            let r = tr.poll();
            if r.more {
                mores += 1;
            }
            if r.error.is_some() {
                errs += 1;
            }
        }
        assert_eq!(mores, 0, "`more` must never be set on a failing read");
        assert_eq!(
            errs, 20,
            "every failing poll reports the error (the runtime edge-triggers it)"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn short_no_bom_utf16_first_sight_is_held_back_then_detected() {
        // REGRESSION (review #4 finding 14): a no-BOM UTF-16 writer caught after its first 2-byte
        // flush committed UTF-8 for life (on .i39: `"A\0"` then `"B\0C\0…"`). With a NUL in a 1–3
        // byte head the reader holds back; once ≥ 4 bytes exist the heuristic classifies it.
        let d = tmpdir();
        let f = d.join("u16.log");
        fs::write(&f, utf16le("A", false)).unwrap(); // 2 bytes: 41 00
        let mut tr = TailReader::new(&f);
        let held = tr.poll();
        assert!(
            held.exists && !held.ready && held.new_text.is_none(),
            "held back"
        );
        append_bytes(&f, &utf16le("BCDEFGH\n", false));
        let r = tr.poll();
        assert!(r.ready);
        assert!(r.encoding_label.contains("UTF-16"), "{}", r.encoding_label);
        assert_eq!(r.new_text.as_deref(), Some("ABCDEFGH\n"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tiny_plain_first_sight_is_not_held_back_and_a_tiny_nul_file_commits_after_the_window() {
        // The other half of finding 14's rule: a 2-byte `x\n` (no NUL) commits at once — a tiny
        // hand-made file never waits — and a file that STAYS at a NUL-bearing 1–3 bytes commits as
        // UTF-8 after SHORT_HEAD_DEFER_POLLS so it never sits on "loading…".
        let d = tmpdir();
        let f = d.join("tiny.log");
        write_utf8(&f, "x\n");
        let mut tr = TailReader::new(&f);
        let r = tr.poll();
        assert!(r.ready);
        assert_eq!(r.new_text.as_deref(), Some("x\n"));

        let g = d.join("nul.log");
        fs::write(&g, [b'A', 0]).unwrap();
        let mut tg = TailReader::new(&g);
        for _ in 0..SHORT_HEAD_DEFER_POLLS {
            assert!(!tg.poll().ready, "held back during the window");
        }
        let r = tg.poll();
        assert!(r.ready, "committed after the window");
        assert_eq!(r.new_text.as_deref(), Some("A\0"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_from_the_start_reports_missing_and_not_exists() {
        // (review #4 test-quality note: the old assertion only checked `!exists`, the Default.)
        let d = tmpdir();
        let mut tr = TailReader::new(d.join("nope.log"));
        let res = tr.poll();
        assert!(res.missing);
        assert!(!res.exists);
        assert!(!res.ready);
        let _ = fs::remove_dir_all(&d);
    }

    // --- .i48 (review #6 finding 4, DECISIONS R148) ---

    #[test]
    fn a_recreated_file_with_a_constant_banner_and_the_same_identity_restarts_from_the_top() {
        // REGRESSION (review #6 finding 4): on ext4 a delete + recreate reuses the inode. With a
        // constant banner >= 64 bytes and the new file already longer than the old cursor when
        // first seen, identity + head were both "unchanged" and the reader continued at the old
        // offset — NEW-1/NEW-2 were never shown and the first shown line was a fragment. The
        // cursor fingerprint catches it. (On a filesystem that does NOT reuse the inode this test
        // still passes via the identity check, so it is not cfg-gated.)
        let dir = tmpdir();
        let f = dir.join("app.log");
        let banner =
            "#Software: Microsoft Internet Information Services 10.0 -- fixed banner line\n";
        assert!(banner.len() >= HEAD_FINGERPRINT_LEN);
        write_utf8(&f, &format!("{banner}old-1\nold-2\n"));
        let mut r = TailReader::new(&f);
        let first = r.poll();
        assert!(first.new_text.unwrap().ends_with("old-2\n"));
        fs::remove_file(&f).unwrap();
        assert!(r.poll().missing);
        write_utf8(&f, &format!("{banner}NEW-1\nNEW-2\nNEW-3\nNEW-4\n"));
        let back = r.poll();
        assert!(back.reappeared);
        assert_eq!(
            back.new_text.unwrap(),
            format!("{banner}NEW-1\nNEW-2\nNEW-3\nNEW-4\n"),
            "the new file is read from its top, not from the old cursor"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_truncate_behind_a_constant_banner_is_detected_by_the_cursor_fingerprint() {
        // review #5 finding 7 (rated speculative there): truncate + rewrite IN PLACE (same inode,
        // same >= 64-byte banner) refilled past the old cursor within one poll. Head and identity
        // are unchanged; the bytes just before the old cursor are not.
        let dir = tmpdir();
        let f = dir.join("iis.log");
        let banner =
            "#Software: Microsoft Internet Information Services 10.0 -- fixed banner line\n";
        write_utf8(&f, &format!("{banner}old-1\nold-2\n"));
        let mut r = TailReader::new(&f);
        let _ = r.poll();
        write_utf8(&f, &format!("{banner}NEW-1\nNEW-2\nNEW-3\nNEW-4\n")); // truncates + rewrites
        let p = r.poll();
        assert!(
            p.rotated,
            "in-place rewrite behind the banner must read as a rotation"
        );
        assert!(p.new_text.unwrap().contains("NEW-1\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_append_never_trips_the_cursor_fingerprint() {
        // The fingerprint must follow the cursor exactly: many appends of odd sizes, across the
        // 8 KB read chunk and across polls, with no false rotation.
        let dir = tmpdir();
        let f = dir.join("grow.log");
        write_utf8(&f, "start\n");
        let mut r = TailReader::new(&f);
        let _ = r.poll();
        let mut fh = fs::OpenOptions::new().append(true).open(&f).unwrap();
        for i in 0..40 {
            let line = format!("{i:04} {}\n", "y".repeat((i * 37) % 700));
            fh.write_all(line.as_bytes()).unwrap();
            if i % 3 == 0 {
                let p = r.poll();
                assert!(!p.rotated && !p.reappeared, "append {i} read as a rotation");
            }
        }
        let p = r.poll();
        assert!(!p.rotated);
        let _ = fs::remove_dir_all(&dir);
    }
}
