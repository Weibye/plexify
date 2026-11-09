# CLAUDE.md

Plexify standardises a Plex media library: it names files the way Plex reads them and
converts what the household's clients cannot Direct Play. This file applies to every coding
agent in this repository, not only Claude Code.

`Dn` below is entry *n* in [`docs/decisions.md`](docs/decisions.md). Read the entry before
changing what it covers; it names the test that guards it.

## Commands

```bash
cargo build
cargo test                               # all tests; `cargo test <name>` for one
cargo fmt --all -- --check               # `cargo fmt` fixes it
cargo clippy --all-targets --all-features
```

These four commands are the quality gate. CI runs them on Ubuntu with apt's FFmpeg, plus a
`rustsec/audit-check` dependency scan. Work is finished when the **PR's CI** is green, not
when the local run is (D26). Clippy warnings don't fail CI, but don't add new ones.

## Principles

In priority order. Cite the one that applies when you push back.

1. **Less is more.** Fewer words, less code, less architecture. A patch smaller than the
   explanation of why it is needed is usually the right size.
2. **Solve the root cause.** Edge cases that keep coming mean the design is the wrong shape.
3. **Fix the system on the second occurrence.** Before fixing anything, search for the same
   problem elsewhere.
4. **Single source of truth.** One fact, rule or term lives in one place and is linked to.
5. **Never fail silently.** Every error is handled or reaches the user with its context.
6. **Prefer the recoverable way to be wrong.** Refuse rather than guess; never invent a
   value. A refused file can be fixed by hand. A file moved to the wrong place may never be
   noticed.
7. **Measure before you claim.** Numbers, before and after, and say what they were
   measured on.
8. **Name things as the domain does.** Plex's and FFmpeg's terms, named for the user's goal.
9. **Write for the person who returns later.** Comments say why, not what. Code, comments
   and docs describe the current state. Transient framing and issue numbers go in the PR.
10. **One concern per PR.** Anything else is a follow-up.

## Invariants

Break one of these only through a new decision.

- **Every queue transition is a rename.** Never read-then-write
  (D1–D6).
- **A job's id is the v5 UUID of its input path**, never random
  (D2).
- **Never assume one worker per input** (D7).
- **The library only ever sees whole files.** Encode in the work folder, then copy to a
  per-worker `.partial` and rename into place (D10).
- **A destination comes from `naming::render`**, never from editing the source string
  (D19).
- **Only `fix.rs` and `undo.rs` give a library file another media name** (D23, D31).
- **Logs go to stderr, and stdout carries only what a command means to print**
  (D24).
- **Paths are compared library-relative with `/` separators** (`paths::to_forward_slashes`).
- **Directory walks prune with `IgnoreFilter::should_skip_dir`**, not file by file.
- **A test that reads or sets an `FFMPEG_*` variable is `#[serial(env)]`** (D25). Other
  tests on process-wide state are `#[serial]`.

## Map

- **Transcoding:** `scan`/`add` → `work` → `clean`, `status`. Code is in `queue/`, `job/`
  and `ffmpeg/`.
- **Naming:** `validate [--fix]`, `undo`. Code is in `naming/`, `fix.rs` and `undo.rs`.
- **Conformance:** `audit`. `probe/` reports what a file contains; `target/` and
  `targets/*.toml` say what each client accepts.

Every command is a struct in `src/commands/` with `new` and `async execute`, wired into
`Commands` in `main.rs`. The stack is tokio, `anyhow` with `.context`, and `tracing`.

## Working here

- **Never run the binary against a real library.** `work` disables sources and `--fix`
  renames them. Build a fixture with the `media-fixture` skill.
- **Run what you changed.** A change to scanning, naming or renaming is verified by running
  the binary on a fixture and reporting what it printed. Passing tests are not enough.
- **Never weaken an assertion to turn CI green.** If a behaviour depends on the FFmpeg
  version, narrow the assertion or document a minimum version.
- **Record decisions.** A choice that is hard to reverse, surprising, and had real
  alternatives gets the next entry in `docs/decisions.md`. Merged entries are never edited.
  A new entry supersedes an old one.
- **Correct a wrong number where it was published**, and say where it came from.
- **Ask before** pushing, opening PRs, posting on GitHub or adding a dependency.

Roles, cross-review and the Pi are in [`docs/FLEET.md`](docs/FLEET.md). Human setup is in
[`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md).
