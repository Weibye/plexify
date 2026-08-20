---
name: naming-patterns
description: How plexify decides whether a media path is correctly named, and how to add or change a supported layout. Use when touching the validate command, the NamingPatterns table, episode or season parsing, or any work on renaming and normalising library files.
---

# Naming patterns

`NamingPatterns::default()` in `src/commands/validate.rs` is the single source of truth for
what counts as a correctly named file. It is a table, not a code path: adding a supported
layout means adding an entry, and the matching logic stays untouched.

## The target layout

Everything in the library is being driven toward one shape:

```
{Root}/{Series Name} ({Year})/Season {NN}/{Series Name} - S{NN}E{NN} - {Episode Title} [{quality}].{ext}
```

with `Root` one of `Series`, `Anime`, or `Movies`. Concretely:

```
Series/Samurai Jack (2001)/Season 03/Samurai Jack - S03E10 - Jack and the Monks.avi
Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv
```

Rules that hold across every entry:

- Season directories are zero-padded to at least two digits — `Season 6` is wrong,
  `Season 06` is right.
- `SxxExx` markers are uppercase.
- A dash with surrounding spaces separates series name, episode marker, and episode title.
- Quality metadata goes in square brackets and comes after the title, with no dash before
  it: `... - Uno [1080p].mkv`.
- The tree root must appear exactly once in a path. `Series/Veronica Mars/Series/...` is a
  duplicated root and must be reported as an error, never silently rewritten — the correct
  destination is ambiguous.

## Writing a pattern entry

Each entry carries a description, the regex, an example, and a `ContentType`. Constraints
that are easy to get wrong:

- The regex matches the path **relative to the media root**, using **forward slashes**, and
  is anchored `^...$`. On Windows the path must be normalised to forward slashes before
  matching, or every pattern fails.
- Start the pattern at the content root (`^Series/`, `^Anime/`, `^Movies/`).
- Optional TVDB ids appear as `(?:\s*\{tvdb-\d+\})?` immediately after the show name.
- Season directories may carry a suffix — `Season 01 - The Arc Name` — matched with
  `(?:\s*-[^/]*)*`.
- Use `[^/]+` for a single path segment. `.+` will happily swallow separators and match
  across directory boundaries, which is how false negatives creep in.

The `example` field is not decoration: `print_report` shows it to the user as the list of
supported patterns. It must be a real string that its own regex matches. When adding an
entry, assert exactly that in a test.

## Canonical test cases

These come from issue #51 and are the acceptance bar for any renaming work. The fixture
built by the `media-fixture` skill contains all of them.

| Input | Expected |
|---|---|
| `Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv` | `Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv` |
| `Series/Charmed/Season 06/Charmed - S06E17 - Hyde School Reunion.avi` | unchanged — already valid |
| `Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi` | `Series/Scrubs/Season 09/Scrubs - S09E02.avi` |
| `Series/Samurai Jack (2001)/Season 3/Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.and.the.Ancient.Master's.Son.avi` | `Series/Samurai Jack (2001)/Season 03/Samurai Jack - S03E10 - Jack The Monks and the Ancient Master's Son.avi` |
| `Series/Super Best Friends Play - FFX/... - S01E13 (1080p60).webm` | `... - S01E13 [1080p60].webm` |
| `Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01/...` | invalid, reported, not rewritten |

Note the third case: unrecoverable scene-release cruft is discarded rather than guessed at.
Only quality metadata worth keeping (resolution, frame rate) survives.

## Renaming safely

Any code that renames files in a real library has to assume it will be run on a library the
user cannot easily reconstruct:

- Report the proposed destination as a full path from the media root. A bare
  `Season 01/file.mkv` is ambiguous and has previously led to files being moved into
  `Season 6/Season 01/` instead of replacing the season directory.
- Rewriting a season directory means **replacing** that path component, not appending to the
  original path.
- Refuse to overwrite an existing destination.
- Dry-run is the default; actually moving files is opt-in.

## Checking your work

```bash
cargo test --lib commands::validate
bash .claude/skills/media-fixture/make-fixture.sh /tmp/plexify-fixture
cargo run -- validate /tmp/plexify-fixture
```

The fixture's five valid files must report clean and its malformed ones must all be caught.
A change that silences a real issue is a regression even when the test suite is green.
