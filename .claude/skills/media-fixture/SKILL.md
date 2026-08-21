---
name: media-fixture
description: Build a disposable media-library tree for exercising plexify's scan, validate, and work commands by hand. Use whenever you need to run the plexify binary against real files - never point a manual run at an actual media library, because work disables source files and validation fixes rename them in place.
---

# Media fixture

Creates a throwaway directory tree that mimics a Plex library, including the malformed
cases plexify is supposed to detect and repair. Use it instead of a real library for any
manual run of the CLI.

## Build the fixture

```bash
bash .claude/skills/media-fixture/make-fixture.sh /tmp/plexify-fixture
```

The argument is the destination root. It is deleted and recreated on every run, so never
point it at a directory holding anything you care about. The script prints the tree it
built and the commands to run against it.

Files are empty placeholders — `validate` only inspects paths, so it works on them
directly. The transcoding commands do need real media; see below.

## What is in it

Canonical content that must report clean:

- `Series/Charmed/Season 06/Charmed - S06E17 - Hyde School Reunion.avi`
- `Anime/Cowboy Bebop/Season 01/Cowboy Bebop - S01E01 - Asteroid Blues.mkv`
- `Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv`
- `Movies/Marvel Cinematic Universe Collection/Iron Man (2008).mkv`

Malformed content, one directory per defect, drawn from the cases in issue #51:

| Directory | Defect |
|---|---|
| `Series/Breaking Bad {tvdb-81189}/Season 01/` | Lowercase episode marker; the tvdb id on the directory must survive the fix |
| `Series/Firefly/Season 1/` | Lowercase marker plus an unpadded season directory |
| `Series/Elementary/Season 6/` | Season number not zero-padded; missing dash before episode title |
| `Series/Scrubs/Season 9/` | Scene-release naming, dots for spaces, release-group cruft |
| `Series/Samurai Jack (2001)/Season 3/` | Dotted name plus unpadded season, apostrophe in episode title |
| `Series/Super Best Friends Play - FFX/` | Quality metadata in parentheses rather than brackets |
| `Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01/` | Duplicated tree root — must be reported, never auto-fixed |
| `Downloads/` | Covered by `.plexifyignore`; must be skipped entirely |

The fixture also writes a `.plexifyignore` at the root ignoring `Downloads/` and `*.tmp`,
so ignore handling is exercised on every run.

## Running against it

`validate` is read-only and safe:

```bash
cargo run -- validate /tmp/plexify-fixture
```

`scan` and `work` write a queue. Always pass `--work-dir` explicitly — it defaults to the
current working directory, which would otherwise scatter `_queue/` into the repo:

```bash
cargo run -- scan /tmp/plexify-fixture --work-dir /tmp/plexify-queue
```

Do not run `work` against the fixture expecting success: the files are empty, so FFmpeg
fails on every job. To exercise the transcoding path, generate a real (tiny) source first:

```bash
ffmpeg -f lavfi -i testsrc=duration=2:size=320x240:rate=10 \
       -f lavfi -i sine=duration=2 \
       -c:v libx264 -c:a aac -y /tmp/plexify-fixture/Movies/Test/Test.mkv
```

## Cleaning up

```bash
rm -rf /tmp/plexify-fixture /tmp/plexify-queue
```
