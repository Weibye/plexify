---
name: naming-patterns
description: How plexify decides whether a media path is correctly named, and how to add or change a supported layout. Use when touching the validate command, the NamingPatterns table, episode or season parsing, or any work on renaming and normalising library files.
---

# Naming patterns

`src/naming/` is the single source of truth for what a correctly named file looks like. It
does not match paths against a list of accepted shapes; it **parses** a path into fields and
**renders** those fields back into one canonical form.

```text
parse:  Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv
        -> series "Elementary", season 6, episode 8, title "Sand Trap"
render: Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv
```

**A path is correct exactly when rendering its own parse reproduces it.** That is the whole
definition, and it is why validation and renaming are the same code path: the destination is
whatever `render` produces, never a patched-up copy of the input string.

Three files, and the split matters when changing them:

- `naming/parse.rs` — recovery from names other tools wrote. Heuristics live here.
- `naming/render.rs` — the canonical form. One function, no alternatives.
- `naming/mod.rs` — the types, and `assess`, which does parse-render-compare.

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

## Changing what is recognised

Teaching the parser a new form of messy name is a change to `parse.rs`, and usually a change
to a table rather than to logic — `RELEASE_TOKENS` is the clearest example. Teaching it a new
*correct* form is a change to `render.rs`, and should be rare: there is one canonical form
by design, and a second accepted shape brings back the ambiguity this replaced.

Constraints that are easy to get wrong:

- Paths arrive **relative to the media root, with forward slashes**. On Windows they must go
  through `crate::paths::to_forward_slashes` first or every component comparison is wrong.
- **Never invent a value.** A heuristic may drop what it is confident is noise and may leave
  a field empty, but a missing year or an unreadable title means `Unresolvable`, not a guess.
- **Whatever `render` emits must parse back to the same fields.** Otherwise a fix moves a
  file, and the next run moves it again. `what_is_rendered_parses_back_to_the_same_fields`
  and `every_proposed_destination_is_itself_canonical` guard this; keep both passing.
- **Rules apply per path component.** A component with no rule is preserved. The series
  directory is not renamed and a file with no season directory is not moved into one — both
  deliberately conservative, because this runs against a library nobody can reconstruct.
- The series name comes from the **file**, not its directory; the directory is consulted only
  when the filename carries no name at all. A directory may be an abbreviation of the series,
  or simply wrong, and rewriting files to match it would spread that.

Two heuristics are judgement calls worth knowing about, both driven by the cases below: a
leading roman numeral is dropped from an episode title, and a shouted trailing token is read
as a release group — but only in a name that already carried recognised release metadata, so
an ordinary title in capitals survives.

## Canonical test cases

These come from issue #51 and are the acceptance bar for any renaming work. The fixture
built by the `media-fixture` skill contains all of them.

| Input | Expected |
|---|---|
| `Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv` | `Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv` |
| `Series/Charmed/Season 06/Charmed - S06E17 - Hyde School Reunion.avi` | unchanged — already canonical |
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
cargo test --lib naming
cargo test --lib commands::validate
bash .claude/skills/media-fixture/make-fixture.sh /tmp/plexify-fixture
cargo run -- validate /tmp/plexify-fixture
```

The fixture's canonical files must report clean and its malformed ones must all be caught.
A change that silences a real issue is a regression even when the test suite is green.
