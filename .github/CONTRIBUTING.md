# Contributing

## Setup

You need stable Rust ([rustup.rs](https://rustup.rs/)) and FFmpeg on your `PATH`:

- Ubuntu/Debian: `apt install ffmpeg`
- macOS: `brew install ffmpeg`
- Windows: `winget install ffmpeg`

```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
cargo build
```

## Before you open a PR

- Run the quality gate in [`CLAUDE.md`](../CLAUDE.md#commands). A PR is ready when its CI is
  green.
- Follow the principles and invariants in the same file. The reasoning behind each
  invariant is in [`docs/decisions.md`](../docs/decisions.md). Add an entry there when you
  make a decision that is hard to reverse.
- Never test against a real media library. `work` disables source files and
  `validate --fix` renames them. Build a throwaway tree instead
  (`.claude/skills/media-fixture/make-fixture.sh`).
- Keep each PR to one concern. Commit messages are imperative and specific.

By contributing you agree that your work is licensed under the MIT License.
