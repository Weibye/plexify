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

**Transcoded output is written to the work folder, then moved.** `FFmpegProcessor::process_job`
encodes to `job.work_folder_output_path(work_folder)` and only then calls
`move_to_destination`. This keeps a media server from indexing a half-written `.mp4` sitting
next to the source. Preserve the write-then-move ordering.

### 2. Library validation (`validate`)

`NamingPatterns::default()` in `src/commands/validate.rs` is a declarative table of regexes,
each with a description, an example, and a `ContentType`. Adding a supported naming layout
means adding a table entry, not writing new matching code.

The regexes match the path **relative to the media root, with `/` separators**, anchored with
`^`/`$` and rooted at a top-level content directory (`Series/`, `Anime/`, `Movies/`). On
Windows, paths must be normalised to forward slashes before matching or every pattern fails.

`validate` is currently read-only: it reports issues and does not modify the library.

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
