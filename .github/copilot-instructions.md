# Copilot Instructions for Plexify

**Read [`CLAUDE.md`](../CLAUDE.md) first.** It is the single source of truth for this
repository's commands, architecture, and conventions, and it applies to every coding agent
working here, not only to Claude Code. This file previously duplicated that material and
drifted out of step with the code; it now covers only the agent workflow rules that live
outside it.

For contributor setup and the release process, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Quality checks are mandatory before finishing work

Run all four locally and ensure they pass before reporting any task complete:

1. `cargo build` — must succeed
2. `cargo test` — all tests must pass
3. `cargo fmt --all -- --check` — must be clean
4. `cargo clippy --all-targets --all-features` — must not add new warnings

These mirror the CI pipeline. If a check fails, fix the cause and re-run all four. Do not
report work as finished on the strength of a partial run, and do not describe a failing or
unrun check as passing.

A fifth CI job runs `rustsec/audit-check` against the dependency tree. It has no local
equivalent and only reports on the PR, so a green local run is not proof the PR will be
green.

## Verify behaviour, not just compilation

A green test suite is not evidence that a user-facing change works. For any change to how
files are scanned, validated, named, or renamed, build a fixture library and run the real
binary against it before claiming the change is correct — see the `media-fixture` skill in
`.claude/skills/`. Report what the run actually printed.

Never run the CLI against a real media library while testing. `work` disables source files
and renaming operations move files in place.

## Pull requests

Transient information — what changed, what is now faster, what bug this fixes — belongs in
the PR description only. Code, comments, tests, and documentation must describe the current
state of the code and remain valid for its lifetime.

Keep PRs short and focused on one concern.
