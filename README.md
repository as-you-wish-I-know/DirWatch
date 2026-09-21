# DirWatch

DirWatch watches a directory and opens a live, read-only tail view for each new or appended
`.log` / `.txt` (or glob-matched) file. It's a single self-contained application that runs
natively on **Windows, macOS, and Linux** from one GUI codebase.

Point it at a folder, and every matching file that appears or grows gets its own live tail
window — no manual `tail -f`, no re-opening files as they rotate.

## Install

Download the build for your platform from the [latest release](../../releases/latest).

| Platform | File | How to run |
|---|---|---|
| **Windows** (x86_64) | `DirWatch-1.0-windows-x86_64.exe` | Self-contained — just run it. No runtime to install. |
| **macOS** (universal) | `DirWatch-1.0-macos-universal.app.zip` | Unzip, then **right-click → Open → Open** the first time (see below). |
| **Linux** (x86_64) | `DirWatch-1.0-linux-x86_64.AppImage` | `chmod +x` then run. Needs FUSE (`libfuse2`) on most distros. |
| **Debian / Ubuntu / Mint** | `dirwatch_1.0.0-1_amd64.deb` | `sudo apt install ./dirwatch_1.0.0-1_amd64.deb` |

### macOS: opening an unsigned app

The macOS build is not code-signed or notarized, so Gatekeeper warns on first launch. Unzip the
app, then **right-click it → Open → Open** to run it the first time. After that it launches
normally.

### Linux AppImage

```sh
chmod +x DirWatch-1.0-linux-x86_64.AppImage
./DirWatch-1.0-linux-x86_64.AppImage
```

If it won't mount, install FUSE (`sudo apt install libfuse2`) or run with
`--appimage-extract-and-run`.

### Verifying your download

Every release ships a `SHA256SUMS` file. Verify with `sha256sum -c SHA256SUMS` (Linux),
`shasum -a 256 -c SHA256SUMS` (macOS), or `certutil -hashfile <file> SHA256` (Windows).

## Usage

With no arguments, DirWatch watches the current directory:

```
DirWatch [directory] [options]
```

| Option | Meaning |
|---|---|
| `[directory]` | Directory to watch (positional; default: current directory). |
| `-d, --depth <n>` | How many subdirectory levels to descend. |
| `-p, --pattern <glob>` | A glob to match (e.g. `*.log`). |
| `--patterns <list>` | A `;`-separated list of globs. |
| `-h, --help` | Show help. |

By default DirWatch matches `.log` and `.txt` files. To watch several directories, launch one
instance per directory.

### Logs

DirWatch writes `DirWatch.log` (errors only, unless `DIRWATCH_DEBUG=1`) and `DirWatch_crash.log`
to the per-user log directory:

- **Windows:** `%LOCALAPPDATA%\DirWatch\`
- **macOS:** `~/Library/Logs/DirWatch/`
- **Linux:** `$XDG_STATE_HOME/dirwatch/` (default `~/.local/state/dirwatch/`)

Each log is capped at 5 MB and rotated once. Pass `--log-dir <path>` to override the location
(useful in a double-clicked launch, which has no command line — put it in the shortcut's target).

## Building from source

DirWatch is a Rust workspace: a platform-agnostic core (`dirwatch-core`) and the GUI
(`dirwatch`, built on [iced](https://iced.rs)).

```sh
cargo build --release
```

The release binary is self-contained: on Windows it statically links the C runtime (no `vcruntime`
/ `msvcp` / `ucrtbase` DLLs); on macOS it links only system frameworks; on Linux it depends only on
the base C library set. GUI libraries (OpenGL / X11 / Wayland) are loaded at runtime, not linked.

Test scripts for each platform (`runtests.sh`, `runtests.ps1`, `runtests.command`) and the Linux
packaging script (`package-linux.sh`) live at the repository root. `PORT-PLAN-crossplatform.md`
describes the architecture.

## License

MIT — see [LICENSE](LICENSE).
