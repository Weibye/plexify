# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                              # Debug build
cargo build --release                    # Release build (LTO, stripped, panic=abort)
cargo test                               # All tests
cargo test --lib                         # Unit tests only (in-module `mod tests`)
cargo test --test integration_tests      # Integration tests only
cargo test test_episode_prioritization   # Single test by name substring
cargo test -- --nocapture                # Show println!/stdout from tests
cargo bench                              # Criterion benchmarks (benches/validate_bench.rs)
```

### Quality gate

CI runs four jobs, and all must pass before a PR merges. Run all four locally before
declaring any task finished, and fix what fails:

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
```

`cargo fmt` (no args) auto-fixes formatting. Clippy warnings do not fail CI, but new ones
should not be introduced. The fourth CI job is a `rustsec/audit-check` dependency scan,
which has no local equivalent.

### Running the CLI against real media

The binary mutates a media library in place — `work` deletes or disables source files, and
validation fixes rename them. Never point a manual test run at a real library. Build a
throwaway tree under a temp directory instead (see the `media-fixture` skill).

## Architecture

Two subsystems live in one binary. They share only the path and naming conventions; no code
path connects them.

### 1. Transcoding pipeline (`scan`/`add` → `work` → `clean`)

**The filesystem is the database.** There is no server, broker, or lockfile daemon. A job is
a JSON file, and the queue is three directories that jobs move between:

```
{work_root}/_queue/{uuid}.job  →  _in_progress/  →  _completed/
                                              ↘  _failed/
```

**A job is named after the file it transcodes.** The `{uuid}` is a v5 UUID derived from the
resolved input path (`Job::id_for_input`), not a random one, and that is what makes the queue
addressable: the same file always maps to the same job filename, so re-scanning a library is
idempotent and both primitives below have something stable to collide on. Never make the id
random again.

Any number of `work` processes on any number of machines can point at the same work root.
Mutual exclusion comes from two filesystem primitives, both in `src/queue/mod.rs`:

- **Enqueue** creates `{uuid}.job.lock` as a *directory*. `create_dir` fails if it exists, so
  the losing racer skips the job rather than double-writing it.
- **Claim** renames the job file from `_queue/` into `_in_progress/`. Rename is atomic, so
  exactly one worker wins; losers see `NotFound` and move on.

Preserve this property when touching the queue. Any change that reads-then-writes instead of
renaming reintroduces the race that this design exists to avoid.

**The work root is not the media root.** `JobQueue::new` takes both. `--work-dir`/`-w`
controls the queue location and **defaults to the current working directory**, not to the
media directory. This trips people up constantly: running `scan` from two different shells
with different CWDs silently produces two unrelated queues. `media_root` is retained on the
queue only for resolving legacy relative job paths.

**Jobs are self-describing snapshots.** `Job::new` resolves the input path to absolute and
bakes the fully-resolved `QualitySettings` into the job file at scan time. A job queued last
week transcodes with the settings it was created with; changing `FFMPEG_CRF` afterwards has
no effect on it. Workers therefore need no shared configuration.

**FFmpeg's command line is positional, so `FFmpegCommandBuilder` assembles it in buckets.**
An option applies to the next file that follows it, and options after the output path are
silently discarded. The builder therefore keeps global, input, and output arguments apart and
joins them in FFmpeg's order at `build` time, so a caller cannot break the command by
chaining in a different order — which is exactly what happened when `with_output` was called
before the stream mappings and every `-map` was thrown away.

Input options are the exception to "order does not matter", and deliberately: they attach
to the **next input declared**, so `.with_subtitle_duration_fix().with_input(source)` puts
the flag on that input alone. Holding them in one bucket ahead of every input, as an earlier
version did, silently put every option on input 0 — which is how the chunked join came to
read its subtitles without `-fix_sub_duration` while the one-pass encode used it.

**Transcoded output is written to the work folder, then moved.** `FFmpegProcessor::process_job`
encodes to `job.work_folder_output_path(work_folder)` and only then calls
`move_to_destination`. This keeps a media server from indexing a half-written `.mp4` sitting
next to the source. Preserve the write-then-move ordering.

**A job that a worker stops running comes back, and a job that cannot succeed stops coming
back.** Both are properties of the same three moves, and both are easy to undo by accident:

- **A claim protects the job, and `ClaimedJob` is the claim.** `try_claim_job_file` writes a
  `{uuid}.job.heartbeat` before it returns, and the returned `ClaimedJob` owns the task that
  refreshes it every `HEARTBEAT_INTERVAL` and aborts it on drop. Both halves have to stay in
  the queue rather than in a caller. Claiming is a rename and rename keeps the mtime, so a job
  that waited out a backlog is *already* older than `STALE_AFTER` when it is claimed; if the
  first heartbeat were the caller's job, the gap before it landed would be a window in which
  two workers could hold one file. The heartbeat is a *separate* file because touching the job
  file itself risks a half-written job.
- **`JobQueue::reclaim_stranded_jobs` runs at worker startup** and renames back to `_queue/`
  any job quiet for `STALE_AFTER`. Nothing else ever looks in `_in_progress/`, so this is the
  only thing that recovers a job from a worker that was killed. It must stay a rename, or two
  workers can both take one job.
- **Every way a job can stop counts as an attempt.** `ClaimedJob::fail` counts one, and so
  does the sweep - a job that takes its worker down (an OOM on a large encode, a panic) never
  reaches `fail`, and without counting there it would cycle forever at the cost of a worker
  each time round. Past `MAX_ATTEMPTS` both paths rename into `_failed/` instead of `_queue/`.
  The count lives in the job file rather than the name, because the name is the v5 id and is
  what makes the queue addressable. `job_exists` consults `_failed/`, so re-scanning cannot
  walk a parked job back into the queue; moving the file out by hand is what asks for a retry,
  and `clean` empties it along with everything else - after saying so.

**`clean` still empties all four directories, and that is why it has to ask first.** The four
are not equally reconstructible: `_queue/` is rebuilt by re-running `scan`, but `_completed/`
is the only record that a file was transcoded, `_failed/` holds every parked job's attempt
count and error *and* is what keeps `job_exists` from re-queueing it, and `_in_progress/`
holds live claims. Narrowing the default set would silently change what the command has always
meant, so instead `src/commands/clean.rs` reports what is in each directory, itemising the two
whose contents nothing can rebuild, and removes nothing until the user says yes. `--only`
narrows a run, `--dry-run` prints the report and stops, `--yes` answers in advance.

Two of those refusals are not questions, and must not become questions:

- **A run with nobody to answer refuses rather than blocks.** `Confirmation::is_interactive`
  is checked before the prompt, so `plexify clean` in a script or a CI job exits with the
  reason instead of waiting on a `read_line` that will never return.
- **A live claim takes more than a `yes`.** A claim whose worker is still checking in - judged
  by `queue::is_stale` and `STALE_AFTER`, the same rule the sweep uses, so the two can never
  disagree about which claims are held - is refused outright, and `--yes` does not cover it.
  `--force` is separate for exactly this reason: deleting a claim mid-encode leaves a worker
  writing an output nothing will reconcile, which is a corruption rather than a cleanup.

**The live-claim refusal runs again, against the disk, immediately before the deletion**, for
the same reason `src/fix.rs` rechecks every proposal before applying it: the report was read
before the user was asked, and the gap is however long a person spends reading it, so a worker
claiming a job inside that window is routine rather than a race. The other counts in the report
are a floor - whatever arrives in the window is deleted too - and that is accepted, because
`_queue` is rebuilt by `scan` and the other two are what the user consented to losing. Only the
live claim is catastrophic, so only the live claim is re-read. The recheck must stay a re-plan
rather than a second walk of its own: two implementations of "live" that can disagree are not a
safeguard.

**A job file that will not parse is still a job file that is about to be deleted.** In
`_failed` that is a parked job somebody needs to see before emptying the directory - a
documented state once `quarantine_unreadable` puts one there deliberately - and in
`_in_progress` it is a claim, because liveness comes off the job file's and the heartbeat's
timestamps and needs no parse. Skipping unreadable jobs is how a live worker's claim walked
past the refusal: it never became a claim, so there was nothing to refuse. They are reported by
their own filename, which is the v5 id and so is stable, and nothing is invented for the fields
that could not be read.

The report and the prompt are `eprintln!` to stderr, not `tracing`. `clean` has no
machine-readable stdout to protect, the report exists to be read next to the question it is
asking, and a confirmation prompt that `RUST_LOG` can filter out is a prompt that can go
missing.

Whatever an interrupted encode left in the work folder is left alone by the sweep. Deciding
what of it is still usable belongs to the encoder, not the queue. What must *not* be left
behind is a live process: `kill_on_drop(true)` on the FFmpeg command is what stops a
cancelled encode from running on and writing the same work-folder path as the worker that
later reclaims the job.

**A long encode is resumable, and that is what the chunk directory in `_in_progress/` is.**
A source over `MIN_CHUNKED_SECONDS` is encoded in `CHUNK_SECONDS` pieces into
`{job-id}.chunks/`, each written as `{n}.ts.part` and renamed to `{n}.ts` only after FFmpeg
exits successfully. A chunk file existing is therefore the whole record of progress —
`encode_chunks` skips what is already there — so nothing may create one by any other route.
The pieces are transport streams because that is the container built to be concatenated;
separately encoded MP4s each carry their own encoder delay and the audio walks further out of
sync at every join. Subtitles are held out of the chunks and muxed in during the join, from
the source, so that no subtitle event is cut in half at a boundary. `reclaim_stranded_jobs`
leaves the chunk directory alone for exactly this reason.

**The concat list declares each chunk's length, and that is load-bearing.** A chunk never
ends exactly where `-t` asked it to, because video cuts on a frame boundary and audio on a
1024-sample AAC frame boundary. Without a `duration` line the demuxer starts each chunk
where the previous one actually ended, and since the error is *per boundary* it compounds:
measured on a three-hour source, the picture ended 350ms behind where it belonged, while a
fifteen-minute episode crossing two boundaries showed nothing at all. Any test short enough
to be comfortable will therefore pass whether or not this is right - `concat_list_entry` is
the only thing keeping it right.

Two things follow from the directory being named after the job id, which is the v5 UUID of
the input path and so stable forever. A parked job's chunks would be found again by any later
job for the same file, so the chunk directory records the `QualitySettings` it was filled
with and `prepare_chunk_dir` discards anything that does not match — half an output at one
CRF and half at another, with nothing recording it, is worse than redoing the work. And
because `job_exists` consults `_failed/`, nothing will ever come back for a parked job at
all, so `discard_work` removes its chunks when it is parked rather than leaving them to
occupy the size of the finished output indefinitely.

**De-prioritising a background worker has no portable spelling.** `nice` is a POSIX utility
and is not on a Windows PATH, so wrapping the command in it there does not lower FFmpeg's
priority — it fails to spawn, and the job fails on every retry. `FFmpegProcessor::ffmpeg_command`
keeps the two behind `cfg`: `nice -n 19` off Windows, `IDLE_PRIORITY_CLASS` as a creation flag
on it. Both worker nodes matter; do not collapse this back to one branch.

### 2. Library validation (`validate`)

`src/naming/` owns what a correct path looks like, and `validate` is a caller. The module
**parses** a library-relative path into fields and **renders** those fields back into one
canonical form; a path is correct exactly when rendering its own parse reproduces it. There
is no table of accepted shapes, and adding one would reintroduce the problem this design
exists to avoid: a yes/no matcher can reject a path but cannot say what it should have been,
so a destination ends up patched together from the source string.

Consequences to preserve when changing it:

- A destination must always come out of `render`, never out of string surgery on the input.
- Anything `render` emits must parse back to the same fields, or a fix would move a file
  twice. There is a test for this; keep it passing.
- Recovery heuristics may drop what they are confident is noise and may leave a field empty,
  but must never invent a value. Unrecoverable paths return `Unresolvable` with a reason.
- Rules apply per path component, and a component with no rule is preserved. The series
  directory is not renamed.
- The season directory is the exception: its **number** comes from the episode's own marker,
  not from the directory. A file with no season directory is moved into one, a misfiled file
  moves across, and season zero renders as `Specials`. An arc name already on that directory
  (`Season 02 - The Mighty Nein`) is kept, because Plex reads the season from the filename
  marker and the arc name is curated information - but only where the directory already
  agrees with the marker, since a file moving seasons cannot carry the old arc name along.

**A run can be narrowed without changing what canonical means.** `naming::scope_for` splits
the path a user gave into a `library_root` (the parent of the outermost `Series`/`Anime`/
`Movies` component) and a `scan_path` (what to walk). Validation walks the scan path and
judges every file against the root, because a path starting `Season 06/` names no series and
would be unresolvable. Two consequences that are easy to break:

- `fix` resolves destinations from `library_root`, never `scan_path`. A destination routinely
  falls outside the scanned subtree — correcting `Season 6` to `Season 06` moves a file into
  a sibling directory.
- `IgnoreFilter` is built from `library_root`, so scoping into a directory cannot override a
  `.plexifyignore` rule written at the root. `for_scope` loads only the rules that can reach
  the scanned subtree - those above it and inside it - so a narrow run stays narrow instead of
  walking the library to find rules that could never match.

Paths are matched **relative to the media root, with `/` separators**. On Windows they must
go through `crate::paths::to_forward_slashes` first, or every component comparison is wrong.

`validate` reports a destination for each file and changes nothing. `validate --fix` carries
those renames out, and `src/fix.rs` is the only code in the project that moves a file in the
library. Its rules exist because it runs against a library nobody can reconstruct:

- Every proposal is rechecked against the disk immediately before it is applied; the report
  was computed earlier and the library may have moved on.
- A destination is never overwritten, and two sources are never allowed to claim one
  destination. Both are refused and reported rather than resolved by guessing.
- A media file and the files named after it - subtitles, `.nfo` - move as a group or not at
  all, so a rename cannot silently break the `.vtt` pairing `work` depends on.
- The plan is written to disk before the first rename, then rewritten with the outcome, so
  an interrupted run leaves a record of what it intended and how far it got.
- Directories the run empties are reported, never removed.

`src/undo.rs` reverses a run from that record, and is the only other code that moves a
library file. It reverses only what the record says was *applied*, checks every reversal
against the disk first, and refuses on three counts: the file is gone from where the fix put
it, something else now holds the original path, or the file's size no longer matches what was
recorded. The record therefore has to carry the media root and the size of each moved file -
without them it is a log, not something that can be acted on. An undo writes its own record,
so it is reversible in turn.

### Cross-cutting

**`.plexifyignore`** (`src/ignore.rs`) is gitignore-like, supports nesting, and child patterns
take precedence over parent ones. `IgnoreFilter` exposes two methods and both matter for
performance: `should_ignore(path)` filters individual files, while `should_skip_dir(path)`
prunes whole subtrees during traversal so ignored directories are never walked. Use
`should_skip_dir` in any new directory walk.

**Environment variables are read as global state.** `Config::from_env` and
`QualitySettings::from_env` call `std::env::var` deep in the call stack rather than receiving
injected config. Consequence for tests: any test that sets, clears, or *reads* an
FFMPEG-prefixed variable must hold `ENV_TEST_MUTEX` (`src/job/mod.rs`), because Rust runs
tests in parallel threads of one process and env vars are process-wide. A test that reads
these variables without the mutex will pass alone and fail intermittently under `cargo test`.

Recognised variables: `FFMPEG_PRESET`, `FFMPEG_CRF`, `FFMPEG_AUDIO_BITRATE`, `SLEEP_INTERVAL`,
`RUST_LOG`.

**Async is tokio throughout**; errors are `anyhow::Result<T>`; logging is `tracing`, initialised
in `main.rs` with the `plexify=info` default filter.

## Conventions

- Every command is a struct in `src/commands/` with `new(...)` and an `async execute()`, wired
  into the `Commands` enum in `src/main.rs`.
- `src/lib.rs` re-exports the modules so integration tests and benches can use them; a module
  must be listed there and in `main.rs` to be visible to both.
- Use `PathBuf`/`Path` for all filesystem work, never string concatenation.
- Integration tests build isolated trees with `tempfile::TempDir`. Tests that depend on
  process-wide state use `serial_test`.

## Pull requests

Transient information — "X is now faster", "this fixes the bug from last week" — belongs in
the PR description only. Code, comments, tests, and documentation must describe the current
state of the code and stay valid for its lifetime. Keep PRs short and focused.
