# Plexify

[![CI](https://github.com/Weibye/plexify/workflows/CI/badge.svg)](https://github.com/Weibye/plexify/actions/workflows/ci.yml)

Plexify standardises a Plex media library. It names files the way Plex reads them, and
converts the files the household's clients cannot Direct Play into MP4s they can. It judges a
file by asking what it contains and what each client accepts, refuses what it cannot decide
rather than guessing, and never changes the library without being asked to.

```bash
plexify validate /media                       # what is not named the way Plex reads it
plexify validate /media --fix                 # carry out the renames it proposed
plexify undo plexify-fix-1787309398.json      # say what reversing a fix would do; --apply does it
plexify audit /media --target lg-cx-webos     # what each file needs to Direct Play there
plexify scan /media --target lg-cx-webos      # queue that work
plexify add /media/Movies/Film/Film.mkv       # queue one file, by its extension
plexify work /media                           # convert what is queued
plexify status                                # what is in the queue, and what is stuck
plexify clean                                 # empty the queue, after asking
```

## Requirements

- **FFmpeg and FFprobe** on `PATH`. `audit` and `scan --target` need FFprobe; `work` needs
  both. `apt install ffmpeg`, `brew install ffmpeg` or `winget install ffmpeg`.
- **Rust 1.85** or newer to build it (`rust-version` in `Cargo.toml`). The test suite needs
  1.93.1, because `serial_test` does.

## Installation

```bash
cargo install --path .
```

### The Raspberry Pi binary

Every CI run builds a static `aarch64-unknown-linux-musl` binary and attaches it to the run as
the artifact `plexify-aarch64-unknown-linux-musl`. It needs no Rust toolchain and no
particular glibc on the Pi.

```bash
scp plexify pi@your-pi:/home/pi/plexify
ssh pi@your-pi 'chmod +x /home/pi/plexify && /home/pi/plexify audit /path/to/media --target lg-cx-webos'
```

`audit` only reads. `validate --fix`, `undo --apply` and `work` change a library, so point
them at a copy before one that matters.

## Output and logs

Logs go to stderr. Stdout carries only what a command means to print: the reports of
`validate`, `audit`, `status` and `undo`. A report can be piped or saved on its own, and
turning up the log level does not corrupt it.

```bash
plexify validate /media > naming-report.txt
RUST_LOG=plexify=debug plexify work /media    # default: plexify=info
```

At debug level `work` logs every FFmpeg command it runs.

## Naming

`validate` reads the library and reports what is not in canonical form. It changes nothing.

```
Series/Show Name/Season NN/Show Name - SNNENN - Episode Title [quality].ext
Movies/Film Name (Year)/Film Name (Year) [quality].ext
```

`Anime/` follows the `Series/` form. The rules, and how a messy name is read, are in the
[naming-patterns skill](.claude/skills/naming-patterns/SKILL.md). Each file is reported one of
two ways: a **proposed rename**, derived from the file's own name, or **needing a decision**,
where the name cannot be decomposed and nothing is proposed.

The path can be the library root or any directory in it. Judgement is always made against the
library root, which plexify finds by looking for `Series`, `Anime` or `Movies` in the path, and
says which it settled on. When the path cannot say, pass `--library-root`. Fixing one series
first is the safe way to start:

```bash
plexify validate "/media/Series/Elementary" --fix
```

`--fix` applies exactly the proposed renames the report listed. It will not:

- overwrite anything, or let two files claim one destination;
- separate a media file from the files named after it (`X.vtt`, `X.en.srt`, `X.nfo`): the
  group moves together or not at all;
- delete anything. Directories a run leaves empty are listed, not removed.

Each run writes `plexify-fix-<timestamp>.json` in the current directory before the first
rename, and rewrites it with what happened. `undo` reverses only what that record says was
applied, and refuses a file that has moved, whose original path is taken, or whose size has
changed. It writes its own record, so an undo can be undone.

## Direct Play

`audit` probes each file with FFprobe and judges it against one client's envelope. It reports
and changes nothing.

```bash
plexify audit /media --target chromecast-gen2-3
plexify audit /media --target ./my-device.toml
```

Envelopes live in `targets/` as TOML. Each value records whether it was `observed` on the
device or `assumed` from a specification, and the report marks a verdict resting on an
assumption with `?`. Results are grouped by cost: a remux copies the video and swaps the
container, audio or subtitles; a re-encode rebuilds the video.

## Converting

`scan` queues a job for every `.webm`, `.mkv` and `.avi` under a directory; `add` queues one
file. `work` runs the queue, puts each MP4 beside its source, and renames the source (and a
WebM's `.vtt`) to `.disabled` rather than deleting it.

What a job does depends on how it was queued:

- **`scan --target`** (repeatable) probes each file and asks every named client. A file every
  client already Direct Plays gets no job. Otherwise the job is the most expensive work any of
  them needs: a remux that copies the video and changes only the tracks that are wrong, or a
  re-encode.
- **Without `--target`**, the extension decides: an AVI is remuxed with everything copied, and
  WebM and MKV are re-encoded.

A WebM needs a `.vtt` of the same name, which becomes its subtitle track. A file with no video
stream is refused.

### How streams are mapped

Take an MKV with one video stream, two AC3 tracks (stereo and 5.1) and two text subtitles,
queued without `--target`. `work` runs:

```bash
ffmpeg -hide_banner -loglevel level+warning -y -fflags +genpts -i Example.mkv \
  -c:v libx264 -preset veryfast -crf 23 -c:a aac -b:a 128k \
  -movflags +negative_cts_offsets -c:s mov_text \
  -map 0:V -map 0:a -map 0:s:0 -map 0:s:1 \
  .../_in_progress/<job-id>_Example.mp4
```

- `0:V` is the picture. The capital `V` leaves out cover art, which an MKV stores as a video
  stream.
- `0:a` takes **every** audio track, so a commentary or second language survives, with its
  language tag. Both come out as AAC, at their own channel counts.
- Subtitles are named one by one, because the source may carry one MP4 cannot hold.
- A remux copies instead: `-c:v copy -movflags +faststart`, and `-c:a copy` unless a target
  needs the audio changed.

Measured with FFmpeg 9.0 on Windows: the output carried h264, two AAC tracks (2 and 6
channels, both `eng`) and two `mov_text` tracks (`eng`, `jpn`).

### Bitmap subtitles

PGS (Blu-ray), VobSub (DVD), DVB, teletext and XSUB subtitles are pictures of text, and MP4 carries
subtitles as text. Such a track is left out of the map and the job goes on: a source with
subtitles `subrip`, `hdmv_pgs_subtitle`, `ass` maps `0:s:0` and `0:s:2`. A warning names the
dropped track, and a louder one says when it was a forced track. The track survives in the
`.disabled` source. When a target needs subtitles out of the container, text tracks are
written beside the MP4 as sidecars, and bitmap tracks are left in the source the same way.

### Quality

`--preset` sets the video and audio quality of the jobs `scan` and `add` create:

| Preset      | x264 preset | CRF | AAC bitrate |
|-------------|-------------|-----|-------------|
| `fast`      | veryfast    | 23  | 128k        |
| `balanced`  | medium      | 20  | 192k        |
| `quality`   | slow        | 18  | 256k        |
| `ultrafast` | ultrafast   | 28  | 96k         |
| `archive`   | veryslow    | 15  | 320k        |

With no preset, the `fast` values apply. `FFMPEG_PRESET`, `FFMPEG_CRF` and
`FFMPEG_AUDIO_BITRATE` override a preset's values. They are read when a job is created and
stored in it, so setting them for `work` changes nothing. `SLEEP_INTERVAL` is how many seconds
an idle worker waits before looking at the queue again (default 60).

## The queue

`scan`, `add`, `work`, `status` and `clean` take `--work-dir` (`-w`), where the queue lives.
**It defaults to the current directory, not the media directory**, so give every command
the same one. Several workers, on one machine or many, can share one queue; each claims a job
by renaming its file. Job files record absolute media paths, so every
machine must reach the media at the same path.

```
<work-dir>/
├── _queue/          waiting
├── _in_progress/    being worked, with a .heartbeat each and a long encode's chunks
├── _completed/      done
└── _failed/         parked; each job file records its last error
```

A failed job is retried, up to three attempts in all, then parked in `_failed/`. Later scans
leave a parked job alone. Move its file back to `_queue/` once the cause is fixed. `clean`
empties `_failed/` too, so read it first.

`work --priority episode` works through one series at a time, episodes in marker order, and
everything else after all episodes. The order between series is unspecified.

`work --background` runs FFmpeg and FFprobe at the lowest priority: `nice -n 19` on Linux and
macOS, `IDLE_PRIORITY_CLASS` on Windows.

### Long sources

A re-encode of a source fifteen minutes or longer is done in five-minute chunks of picture, written
to `_in_progress/<job-id>.chunks/`. The chunks are then joined, and the audio and subtitles
are taken from the source in that same pass. A worker that picks up an interrupted job carries
on from the last finished chunk. During the join the work directory holds both the chunks and
the joined MP4.

### Stopping a worker

Ctrl-C stops `work`. An encode in progress is killed, and its job stays in `_in_progress/`.
Ctrl-C while the worker waits for jobs takes effect when the wait ends.

Only a worker that is starting returns abandoned jobs to the queue, and only once they have
been quiet for five minutes, so a worker restarted straight away leaves the job where it is.
The return counts as a failed attempt, so a job interrupted three times is parked.

Nothing else is handled. `SIGTERM` ends the worker at once, without killing an FFmpeg it
started unless the signal reached that process too. Its job is recovered the same way.

## .plexifyignore

A `.plexifyignore` file, at any level of the library, keeps paths out of `scan`, `validate`
and `audit`. Patterns are gitignore-like, and the last one to match a path decides, with a
deeper file's patterns read after its parent's:

```
# a directory, at any depth
Downloads/
# only beside this .plexifyignore
/temp_file.mkv
*.tmp
# re-include what an earlier pattern ignored
!keep.tmp
```

An ignored directory is not walked, so nothing inside it can be re-included.

## Development

[`CLAUDE.md`](CLAUDE.md) has the commands, the quality gate and the principles, and
[`docs/decisions.md`](docs/decisions.md) has why the code is the way it is. Human setup is in
[`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md).
