# Plexify

[![CI](https://github.com/Weibye/plexify/workflows/CI/badge.svg)](https://github.com/Weibye/plexify/actions/workflows/ci.yml)

A simple, distributed media transcoding CLI tool that converts .webm, .mkv and .avi files to .mp4 format with subtitle support, optimized for Plex media servers. An .avi is remuxed with its video copied rather than re-encoded.

## Features

- **Distributed Processing**: Queue-based system allows multiple workers to process jobs concurrently
- **Subtitle Support**: External .vtt subtitles for .webm files, and every embedded subtitle track for .mkv and .avi files
- **Background Processing**: Run workers in low-priority background mode
- **Configurable**: Customizable FFmpeg settings via environment variables
- **Atomic Job Processing**: Race condition-free job claiming for multiple workers
- **Signal Handling**: Graceful shutdown on SIGINT/SIGTERM
- **Cross-Platform**: Works on Linux, macOS, and Windows
- **Modern Architecture**: Built with Rust for safety, performance, and maintainability

## Requirements

- **FFmpeg**: Required for media transcoding
- **Rust**: Version 1.70+ (for compilation) - pre-built binaries available

### Installing FFmpeg

**Ubuntu/Debian:**
```bash
sudo apt update
sudo apt install ffmpeg
```

**macOS:**
```bash
brew install ffmpeg
```

**Windows:**
```bash
winget install ffmpeg
```

**CentOS/RHEL:**
```bash
sudo yum install epel-release
sudo yum install ffmpeg
```

## Installation

### Option 1: From Source

1. Clone the repository:
```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
```

2. Build with Cargo:
```bash
cargo build --release
```

3. The binary will be available at `./target/release/plexify`

4. Optionally, install to system PATH:
```bash
cargo install --path .
```

### Option 2: The Raspberry Pi binary

Every CI run builds a statically linked `aarch64-unknown-linux-musl` binary and
attaches it to the run as an artifact named
`plexify-aarch64-unknown-linux-musl`. It is static, so it needs no Rust
toolchain, no glibc of a particular version, and nothing else installed on the
Pi. `audit` additionally needs `ffprobe` on the machine holding the files;
`work` needs `ffmpeg` as well.

Download it from the Actions run, then:

```bash
# from the machine that downloaded it
scp plexify pi@your-pi:/home/pi/plexify
ssh pi@your-pi 'chmod +x /home/pi/plexify && /home/pi/plexify --version'
```

To read a library without changing anything on it:

```bash
ssh pi@your-pi '/home/pi/plexify audit /path/to/media --target lg-cx-webos'
```

`audit` only reads. `scan`, `work` and `validate --fix` change a library, so
point those at a copy before pointing them at one that matters.

### Option 3: Pre-built Binaries for everything else (Coming Soon)

Binaries for Linux, macOS, and Windows, published to GitHub releases and
updated in place, are not built yet.

## Usage

### Basic Commands

```bash
# Scan a directory for media files and create transcoding jobs
# Recursively scans all subdirectories for .webm, .mkv and .avi files
plexify scan /path/to/media

# Scan with a quality preset for consistent encoding settings
plexify scan --preset quality /path/to/media

# Ask a client what each file needs before queueing anything. A file it
# already Direct Plays produces no job at all.
plexify scan --target lg-cx-webos /path/to/media

# Name every client the library has to serve. Each file is queued with the
# most expensive work any of them needs, because that is the only answer
# that satisfies all of them.
plexify scan --target lg-cx-webos --target chromecast-gen2-3 /path/to/media

# Process jobs from the queue (foreground)
plexify work /path/to/media

# Process jobs in background with low priority
plexify work /path/to/media --background

# Process jobs with episode prioritization (series episodes first, in order)
plexify work /path/to/media --priority episode

# Clean up temporary files
plexify clean /path/to/media

# Report what in the library is not in canonical form
plexify validate /path/to/media

# Carry out the renames it proposes
plexify validate /path/to/media --fix

# Report what each file needs before a client will Direct Play it
plexify audit /path/to/media --target chromecast-gen2-3
```

### Direct Play audit

`audit` probes each file with FFprobe and judges what it finds against one
client's playback envelope. It reports and changes nothing.

```bash
plexify audit /path/to/media --target chromecast-gen2-3
plexify audit /path/to/media --target lg-cx-webos
plexify audit /path/to/media --target ./my-device.toml
```

Results are grouped by what fixing them costs, and the two are never added
together: a remux swaps an audio or subtitle track and copies the video
bitstream across, while a re-encode rebuilds the video and, on a Raspberry Pi,
runs slower than realtime.

Envelopes live in `targets/` as TOML. Every value in one records whether it was
`observed` on the hardware or `assumed` from a specification, and the report
marks any verdict resting on an assumption with `?`. Three spec-derived
assumptions in this project have already turned out to be wrong on the device,
so a claim nobody has watched happen is not allowed to look like a measurement.

### The work directory

`scan`, `add`, `work`, and `clean` all take `--work-dir` (short form `-w`), which sets where
the job queue lives. **It defaults to the current working directory, not the media
directory.**

```bash
# Queue is created under /srv/plexify-queue
plexify scan /path/to/media --work-dir /srv/plexify-queue
plexify work /path/to/media --work-dir /srv/plexify-queue
```

Every command that touches the queue must be given the same work directory, otherwise it
will look at an empty one. Because the default is the current working directory, running
`plexify scan /media` from two different shells creates two unrelated queues, and a worker
started from a third directory finds no jobs at all. Pass `--work-dir` explicitly whenever
more than one command or machine is involved.

`clean` removes the queue directories from the work directory, so it needs the same
`--work-dir` as the `scan` that created them.

For distributed processing, point every machine's `--work-dir` at the same shared location:

```bash
# Machine A
plexify scan /mnt/media --work-dir /mnt/shared/plexify-queue

# Machine B, reading the same queue
plexify work /mnt/media --work-dir /mnt/shared/plexify-queue --background
```

Job files record absolute media paths, so each machine must be able to reach the media at
the same path.

### Hierarchical Directory Support

Plexify automatically scans through your entire media directory hierarchy, finding media files in any subdirectory structure:

```
/media/
├── Movies/
│   ├── Action/
│   │   └── movie1.mkv
│   └── Comedy/
│       └── movie2.webm
│       └── movie2.vtt
├── TV Shows/
│   ├── Show1/
│   │   ├── Season 1/
│   │   │   └── episode1.webm
│   │   │   └── episode1.vtt
│   │   └── Season 2/
│   │       └── episode2.mkv
│   └── Show2/
│       └── episode.mkv
└── Documentaries/
    └── doc1.mkv
```

Running `plexify scan /media` will find and queue jobs for **all** media files regardless of their depth in the directory structure.

### Episode Prioritization

Plexify supports intelligent job prioritization for TV series episodes:

```bash
# Process jobs with episode prioritization
plexify work /path/to/media --priority episode

# Default behavior - process jobs in order found
plexify work /path/to/media --priority none  # or just omit --priority
```

**Episode Priority Mode:**
- **Series episodes are processed first**, sorted alphabetically by series name
- **Within each series**, episodes are processed in ascending order (S01E01, S01E02, S01E03...)
- **Non-episode content** (movies, etc.) is processed after all episodes
- **Perfect for binge-watching scenarios** - get your episodes in the right order

**Example processing order with `--priority episode`:**
1. Series/Better Call Saul/Season 01/Better Call Saul S01E01 Uno.mkv
2. Series/Better Call Saul/Season 01/Better Call Saul S01E02 Mijo.mkv  
3. Series/Breaking Bad/Season 01/Breaking Bad S01E01 Pilot.mkv
4. Series/Breaking Bad/Season 01/Breaking Bad S01E03 Gray Matter.mkv
5. Movies/The Matrix (1999)/The Matrix (1999).mkv

**Supported episode formats:**
- `Series/Show Name/Season XX/Show Name SxxExx Episode Title.ext`
- `Series/Show Name {tvdb-12345}/Season XX/Show Name SxxExx Episode Title.ext`
- `Series/Show Name/Season XX - Extra Info/Show Name SxxExx Episode Title.ext`
- `Anime/Show Name/Season XX/Show Name SxxExx Episode Title.ext`

### Quality Presets

Plexify includes predefined quality presets for different use cases:

- **`fast`** - Fast encoding with good quality (veryfast/23/128k) - Default behavior
- **`balanced`** - Balanced encoding speed and quality (medium/20/192k) - Recommended
- **`quality`** - High quality, slower encoding (slow/18/256k) - Best for archival
- **`ultrafast`** - Ultra-fast encoding for quick previews (ultrafast/28/96k)
- **`archive`** - Archive quality for long-term storage (veryslow/15/320k)

Examples:
```bash
# Scan with balanced preset (recommended for most users)
plexify scan --preset balanced /path/to/media

# Scan with quality preset for best results
plexify scan --preset quality /path/to/media

# Scan with fast preset for quick transcoding
plexify scan --preset fast /path/to/media
```

### .plexifyignore Support

Plexify supports `.plexifyignore` files to exclude directories and files from scanning and validation. These files work similar to `.gitignore` files and can be placed at any level in your directory tree.

#### Pattern Syntax

- **Basic patterns**: `filename.ext`, `directory_name`
- **Wildcards**: `*.tmp`, `*.log` 
- **Directory patterns**: `Downloads/` (trailing slash matches directories only)
- **Negation**: `!important.mkv` (include files that would otherwise be ignored)
- **Path patterns**: `path/to/file` (relative to the .plexifyignore location)
- **Root patterns**: `/Downloads` (absolute from the .plexifyignore location)

#### Example .plexifyignore

```
# Ignore system directories
Downloads/
InProgress/
lost+found/
tools/

# Ignore temporary and backup files
*.tmp
*.bak
*.old
*.DS_Store
Thumbs.db

# Ignore specific directories but allow important files
old_episodes/
!important_episode.mkv

# Ignore files in root only
/temp_file.mkv
```

#### Usage

1. Create a `.plexifyignore` file in your media root or any subdirectory
2. Add patterns for files/directories you want to exclude
3. Run `plexify scan` or `plexify validate` - ignored paths will be skipped automatically

The `scan` and `validate` commands will show how many paths were ignored:

```
📋 Ignored 15 paths due to .plexifyignore patterns
```

**Note**: Nested `.plexifyignore` files are supported - patterns from parent directories apply to child directories, with child patterns taking precedence.

Environment variables can override preset values:
```bash
# Use quality preset but override CRF to 20
FFMPEG_CRF=20 plexify scan --preset quality /path/to/media
```

### Typical Workflow

1. **Scan**: Create jobs for all .webm, .mkv and .avi files with your preferred quality preset
```bash
# Scan with balanced preset (recommended)
plexify scan --preset balanced /home/user/Videos --work-dir /home/user/plexify-queue

# Or scan with custom settings via environment variables
FFMPEG_PRESET=medium plexify scan /home/user/Videos --work-dir /home/user/plexify-queue
```

2. **Work**: Start processing the queue (you can run multiple workers)
```bash
# Terminal 1 - High priority worker
plexify work /home/user/Videos --work-dir /home/user/plexify-queue

# Terminal 2 - Background worker
plexify work /home/user/Videos --work-dir /home/user/plexify-queue --background
```

Each terminal has its own current working directory, so passing `--work-dir` explicitly is
what makes both workers share one queue.

3. **Monitor**: Check logs for progress (logs output to stdout)

4. **Clean**: Remove temporary files when done
```bash
plexify clean /home/user/Videos --work-dir /home/user/plexify-queue
```

5. **Validate**: See what is not in canonical form, then fix it
```bash
plexify validate /home/user/Videos
plexify validate /home/user/Videos --fix
```

## Library Naming Validation

The `validate` command reads your library and reports what is not in canonical form. It
changes nothing on disk.

```bash
plexify validate /path/to/media
```

### The canonical form

Every file in the library is driven toward one shape:

```
Series/Show Name/Season NN/Show Name - SNNENN - Episode Title [quality].ext
Anime/Show Name/Season NN/Show Name - SNNENN - Episode Title [quality].ext
Movies/Film Name (Year)/Film Name (Year) [quality].ext
```

- Season directories are zero-padded to two digits: `Season 06`, not `Season 6`.
- Episode markers are uppercase: `S06E08`, not `s06e08`.
- A dash with spaces around it separates the show name, the marker, and the title.
- Quality metadata goes in square brackets, after the title, with no dash before it.
- The episode title and the quality are both optional, and disappear cleanly when a name
  does not carry them: `Scrubs - S09E02.avi`.

A series directory may carry a year and a TVDB id — `Breaking Bad (2008) {tvdb-81189}` — and
both are preserved as they are. The series directory is never renamed.

The season directory is the one part of the path that is *decided* rather than kept. It
comes from the episode marker in the filename, so:

- A file with no season directory is moved into `Season NN`, created if it does not exist.
- A file in the wrong season directory moves to the one its marker names.
- Season zero is `Specials`.
- An arc name already on a season directory — `Season 02 - The Mighty Nein` — is kept. Plex
  reads the season from the `SxxExx` marker in the filename, so the arc name costs nothing.
  It is dropped only when a file moves to a different season, since the old season's arc name
  does not describe the new one.

A directory nested below the season directory, such as `Season 01/Extras`, stays where it is.

### Example output

```
📊 Library Naming Report
═══════════════════════
📂 Scanned directory: /home/user/Videos
📁 Files scanned: 12
✏️  Renames proposed: 2
🤔 Needing a decision: 1

✏️  Proposed renames:
─────────────────────

  Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv
→ Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv

  Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi
→ Series/Scrubs/Season 09/Scrubs - S09E02.avi

🤔 Needing a decision:
──────────────────────

  Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01/Veronica Mars S02E04.mp4
  'Series' appears twice in this path; the correct location is ambiguous
```

A file is reported one of two ways. A **proposed rename** is a destination the tool derived
from the file's own name and can act on. **Needing a decision** means the name could not be
decomposed — a duplicated library root, a missing episode marker, a film with no year — and
nothing is proposed, because the right answer is not recoverable from the path.

### Narrowing a run

The path can be the library root or any directory inside it:

```bash
plexify validate /path/to/media                          # the whole library
plexify validate /path/to/media/Series                   # one root
plexify validate "/path/to/media/Series/Elementary"      # one series
plexify validate "/path/to/media/Series/Elementary/Season 6"
```

Judgement is always made against the library root, whichever directory you point at —
plexify finds the root by looking for `Series`, `Anime`, or `Movies` in the path, and says
which one it settled on. That matters because canonical form is defined from the root: a
path starting `Season 06/` names no series and belongs to no root.

This is the safe way to start on a library you care about. Fix one series, let Plex rescan
it, and widen once you trust the result:

```bash
plexify validate "/media/Series/Elementary" --fix
```

Files outside the directory you named are not touched, but destinations may still land
outside it — correcting `Season 6` to `Season 06` moves a file into a sibling directory, and
that is the point.

### Carrying the renames out

```bash
plexify validate /path/to/media --fix
```

The report prints first and `--fix` then applies exactly what it listed. Only proposed
renames are acted on; anything needing a decision is left alone.

What it will not do:

- **Overwrite anything.** If a destination is already occupied, that rename is refused and
  reported.
- **Merge two files into one name.** If two files canonicalise to the same destination —
  two season directories differing only by an arc name, say — both are refused.
- **Separate a file from its subtitles.** A `.vtt`, `.srt` or `.nfo` named after a media
  file moves with it. If any part of the group is blocked, none of it moves.
- **Delete anything.** Directories left empty by a run are listed at the end, not removed.

Every run writes a plan file — `plexify-fix-<timestamp>.json` in the current directory —
before the first rename, and rewrites it afterwards with what actually happened. It records
every move as a `from`/`to` pair, so an interrupted run leaves a record of how far it got.

```
🔧 Fix
──────
✅ Renamed: 8
📄 Plan: plexify-fix-1787309398.json

📁 Left empty by this run, and not removed:
───────────────────────────────────────────
   Series/Elementary/Season 6
```

Running `validate` again after a fix proposes nothing: the destinations it produces are
themselves canonical.

### Putting a fix back

```bash
plexify undo plexify-fix-1787309398.json          # say what would be reversed
plexify undo plexify-fix-1787309398.json --apply  # reverse it
```

Undo reverses only what the fix actually applied, so an interrupted run puts back exactly
what it managed to move. Sidecars come back with their media file.

It is checked against the disk the same way a fix is, and for a stronger reason: a fix acts
on a report seconds old, while an undo acts on a record written however long ago you took to
notice. It refuses, per file, when:

- the file is no longer where the fix put it;
- something else now occupies the path it came from;
- the file is no longer the one that was moved — its size no longer matches what the record
  says. That is weak evidence, and deliberately so: it catches a file having been replaced,
  not a re-encode that happens to be the same length.

An undo writes its own record, `plexify-undo-<timestamp>.json`, so it can be reversed in
turn. Like a fix, it writes what it intends before it starts and replaces that with what it
did — so a run interrupted halfway leaves a file that says what it meant to do, and cannot be
mistaken for a record of what happened.

## Configuration

Plexify offers two ways to configure encoding settings:

### 1. Quality Presets (Recommended)
Use predefined presets for consistent, tested settings:
```bash
plexify scan --preset balanced /path/to/media  # Recommended for most users
plexify scan --preset quality /path/to/media   # Best quality
plexify scan --preset fast /path/to/media      # Fastest encoding
```

### 2. Environment Variables
Override individual settings or customize presets:
```bash
export FFMPEG_PRESET="veryfast"     # FFmpeg preset (default: veryfast)
export FFMPEG_CRF="23"              # Constant Rate Factor (default: 23)
export FFMPEG_AUDIO_BITRATE="128k"  # Audio bitrate (default: 128k)
export SLEEP_INTERVAL="60"          # Sleep between job checks in seconds (default: 60)
```

The `FFMPEG_*` variables are read when a job is created, by `scan` and `add`, and the
resolved settings are stored in the job file. Setting them in a worker's environment has
no effect on jobs that already exist — a job always encodes with the settings it was
queued with. `SLEEP_INTERVAL` is the exception: it belongs to the worker loop and is read
by `work`.

### Combining Presets and Environment Variables
Environment variables override preset values:
```bash
# Use quality preset but with faster encoding preset
FFMPEG_PRESET="medium" plexify scan --preset quality /path/to/media

# Use balanced preset but with higher quality CRF
FFMPEG_CRF="18" plexify scan --preset balanced /path/to/media
```

## File Processing

### .webm Files
- Requires matching .vtt subtitle file (same name, different extension)
- Example: `video.webm` requires `video.vtt`
- Output: `video.mp4` with embedded subtitles

### .mkv Files
- Example: `video.mkv` → `video.mp4`
- Every video, audio and subtitle stream is carried across, not just the first of each, so a
  commentary track or a second language survives the conversion. Stream metadata — language
  tags, dispositions — comes with them.
- A file with no subtitle stream converts normally.
- Bitmap subtitles (PGS from Blu-ray, VobSub from DVD) cannot be stored in MP4 and currently
  fail the job rather than being dropped silently.

## Directory Structure

Plexify keeps its job queue in the **work directory**, which is separate from your media
directory. See [The work directory](#the-work-directory) for how to choose it.

```
/path/to/work/           # --work-dir, defaults to the current working directory
├── _queue/              # Pending jobs
├── _in_progress/        # Currently processing, plus each job's part-encoded chunks
├── _completed/          # Finished jobs
└── _failed/             # Jobs that failed too many times to keep retrying

/path/to/media/          # Untouched except for the transcoded output
├── video1.webm
├── video1.vtt
└── video2.mkv
```

Keeping the queue outside the media tree means your media server never sees Plexify's
bookkeeping files, and lets several machines share one queue over a network mount while
each reads media from its own path.

### When a worker stops

A worker that is interrupted — Ctrl-C, a kill, a machine losing power — leaves its job in
`_in_progress/`. While a worker is running a job it keeps a `.heartbeat` file next to it, so
the next worker to start can tell an abandoned job from one that is still being encoded, and
moves the abandoned ones back to `_queue/`. That sweep runs at startup, and a job has to have
been quiet for five minutes before it is taken back.

Anything the interrupted encode finished is kept. A source longer than fifteen minutes is
encoded in five-minute chunks, and the worker that picks the job up carries on from the last
completed chunk rather than starting the file again.

### When a job cannot succeed

A job that fails is returned to the queue and tried again, three times in all. After that it
is moved to `_failed/`, where the job file records the attempt count and the last error:

```bash
cat /path/to/work/_failed/*.job
```

A parked job is left alone by later scans, so it will not quietly find its way back into the
queue. Once the cause is fixed, move the job file back into `_queue/` to have it tried again.

`plexify clean` is the exception: it empties `_failed/` along with the other queue
directories, so the next scan queues those files again from scratch and the recorded errors
are gone. Read `_failed/` before you clean.

A job that takes its *worker* down rather than failing - an encode that runs the machine out
of memory, say - is counted the same way when the next worker sweeps it back, so it cannot
cycle indefinitely either.

## FFmpeg Processing Details

### For .webm files:
```bash
ffmpeg -fflags +genpts -avoid_negative_ts make_zero \
  -i input.webm -i input.vtt \
  -map 0:v:0 -map 0:a:0 -map 1:s:0 \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -c:s mov_text \
  -y output.mp4
```

### For .mkv files:
```bash
ffmpeg -fflags +genpts -avoid_negative_ts make_zero -fix_sub_duration \
  -i input.mkv \
  -map 0:v:0 -map 0:a:0 -map 0:s:0 \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -c:s mov_text \
  -y output.mp4
```

### For long sources

A source longer than fifteen minutes is encoded a chunk at a time, so that an interrupted
worker leaves something the next one can carry on from. Each chunk is encoded from a seek
into the source and written as a transport stream, which is the container built to be
joined; the chunks are then concatenated into the MP4 without being re-encoded, and the
subtitles are added in that same pass so that no subtitle is cut at a chunk boundary.

```bash
# once per chunk, into {work}/_in_progress/{job-id}.chunks/
ffmpeg -hide_banner -y -fflags +genpts -ss 300 -i input.mkv \
  -avoid_negative_ts make_zero -t 300 \
  -map 0:v -map 0:a \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -muxdelay 0 -muxpreload 0 -f mpegts 00001.ts

# chunks.txt pins each chunk to the length it was cut to, so the joined
# timeline matches the source rather than drifting a few ms per boundary:
#   file '/work/.../00000.ts'
#   duration 300.000

# then, once, to join them
ffmpeg -hide_banner -y -avoid_negative_ts make_zero \
  -f concat -safe 0 -i chunks.txt \
  -fix_sub_duration -i input.mkv \
  -map 0:v -map 0:a -map 1:s? \
  -c:v copy -c:a copy -c:s mov_text output.mp4
```

Note where `-fix_sub_duration` sits: it describes the source the subtitles are read from,
not the list of chunks, so a long source converts its subtitles exactly as a short one
does.

And note the `duration` lines in the list. A chunk never ends exactly where it was asked
to - `-t` cuts video on a frame boundary and audio on a 1024-sample AAC frame boundary, so
each one runs a few milliseconds long. Left to itself the demuxer would start each chunk
where the last actually ended, and because that error is per boundary it accumulates: a
three-hour film crosses thirty-five of them. Declaring the length each chunk was cut to
pins it to the position it came from.

The chunk directory is removed once the join succeeds, and also if the job is given up on
and parked in `_failed/`. It records the quality settings its chunks were encoded with, and
chunks made with different settings are discarded rather than mixed into one output.

While the join runs, the work directory holds both the full set of chunks and the assembled
MP4, so **peak usage is roughly twice the size of the output**. Size the work directory for
that, not for one copy.

### Background workers

`--background` runs FFmpeg at the lowest priority the platform offers: `nice -n 19` on Linux
and macOS, and `IDLE_PRIORITY_CLASS` on Windows.

These are not quite equivalent. A `nice -n 19` process still gets scheduled under Linux's
CFS, while an idle-priority process on Windows can be starved almost completely by ordinary
desktop activity - which is what you want on a dedicated worker, and may be more than you
want on a machine you are also using.

## Distributed Processing

Multiple workers can safely process the same queue:

```bash
# Worker 1 (high priority)
plexify work /media/videos

# Worker 2 (background, low priority)
plexify work /media/videos --background

# Worker 3 (on another machine with shared storage)
plexify work /shared/media/videos
```

Each worker atomically claims jobs to prevent conflicts.

## Signal Handling

Workers handle `SIGINT` (Ctrl+C) and `SIGTERM` gracefully:
- Completes current job before shutting down
- Returns job to queue if interrupted mid-processing
- Immediate shutdown if no job is currently running

## Logging

The Rust version uses structured logging. Control log levels with the `RUST_LOG` environment variable:

```bash
# Default: info level
plexify work /path/to/media

# Debug level for troubleshooting
RUST_LOG=debug plexify work /path/to/media

# Only warnings and errors
RUST_LOG=warn plexify work /path/to/media
```

## Development

### Building from Source

```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
cargo build
```

### Running Tests

```bash
cargo test
```

### Code Quality

The project includes comprehensive CI/CD with:

- **Build & Test**: Automated testing on every PR and push to main
- **Code Formatting**: Enforced via `cargo fmt` 
- **Linting**: Code quality checks via `cargo clippy`
- **Security Audit**: Dependency vulnerability scanning via `cargo audit`

To run quality checks locally:
```bash
# Format code
cargo fmt

# Run linter 
cargo clippy --all-targets --all-features

# Check formatting
cargo fmt --all -- --check
```

### Code Structure

The project is organized into modules:

- `commands/` - CLI command implementations (scan, work, clean)
- `config/` - Configuration management
- `job/` - Job definitions and processing logic
- `queue/` - Job queue management with atomic operations
- `ffmpeg/` - FFmpeg integration
- `worker/` - Worker coordination (extensible for future features)

## Troubleshooting

### Common Issues

1. **"No jobs found"**: 
   - Run `scan` command first
   - Check that .webm files have matching .vtt files

2. **FFmpeg errors**:
   - Verify FFmpeg is installed and in PATH
   - Check file permissions and disk space
   - Enable debug logging: `RUST_LOG=debug plexify work /path`

3. **Permission errors**:
   - Ensure write permissions to media directory
   - Check that temporary directories can be created

### Debug Mode

Enable debug output:
```bash
RUST_LOG=debug plexify scan /path/to/media
```

