---
name: checks
description: Run plexify's full CI quality gate locally - build, test, format, clippy - and report what failed. Use before finishing any code change, before opening or updating a PR, or whenever asked to verify the repository is green.
---

# Checks

Runs the same four jobs CI runs. Work is not finished until all four pass.

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
```

Run all four even if an earlier one fails, so the report covers everything rather than
stopping at the first problem. Then state plainly which passed and which did not, and paste
the actual failure output — never summarise a failure as a pass.

## Interpreting the results

**Build and test** must be clean. A test that fails only sometimes is still a failure; see
below.

**Format** is auto-fixable with `cargo fmt` (no arguments). There is never a reason to leave
this one red.

**Clippy** warnings do not fail CI, so the gate here is comparative: no *new* warnings versus
`main`. Dead-code warnings deserve a closer look rather than an `#[allow]` — in this
repository they have reliably meant a refactor landed only halfway, with a new abstraction
built and the old call sites never migrated.

**Security audit** is the fourth CI job (`rustsec/audit-check`) and has no local equivalent.
It only runs on the PR, so a green local run does not guarantee a green PR.

## Flaky tests

`cargo test` runs tests in parallel threads of a single process, and this codebase reads
FFMPEG-prefixed environment variables as process-wide global state. A test touching those
variables without holding `ENV_TEST_MUTEX` (`src/job/mod.rs`) will pass in isolation and fail
intermittently in a full run.

If a test fails, re-run it two ways before concluding anything:

```bash
cargo test <name>                  # alone
cargo test -- --test-threads=1     # whole suite, serialised
```

Passing alone or serialised but failing in a normal run means the test is racing on shared
state, not that the code under test is broken. Fix the isolation — do not paper over it by
serialising the whole suite.
