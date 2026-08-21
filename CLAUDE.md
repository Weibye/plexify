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

**Transcoded output is written to the work folder, then moved.** `FFmpegProcessor::process_job`
encodes to `job.work_folder_output_path(work_folder)` and only then calls
`move_to_destination`. This keeps a media server from indexing a half-written `.mp4` sitting
next to the source. Preserve the write-then-move ordering.

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
- Rules apply per path component. The series directory is not renamed, and a file with no
  season directory is not moved into one.

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
