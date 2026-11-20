# Contributing to Gitust

Thanks for helping! Bug reports, ideas and pull requests are all welcome.

## Build and test

```sh
cargo run                 # debug build
cargo test                # unit + integration tests (needs `git` on PATH)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Linux also needs `libfontconfig1-dev libxkbcommon-dev`. CI runs fmt, clippy and the
tests on Windows, Linux and macOS for every pull request.

## How the code is organised

- `ui/*.slint` is only the view. All state and decisions live in Rust, and
  `ui/state.slint` is the single contract between the two.
- `src/git.rs` reads repositories with libgit2. It must never run a program.
- `src/ops.rs` holds **every** change to a repository, through the Git CLI with an
  explicit argv (never a shell string) and paths over stdin as literal pathspecs.
  New operations go here, with tests that run them against real temporary
  repositories, including file names with spaces, a leading `-`, Unicode and glob
  characters.
- `src/patch.rs` builds hunk/line staging patches; it is covered by randomized
  tests that compare `git apply` results with an independent model. Keep it that way.
- The UI thread never touches Git, disk or network.

## Pull requests

- Keep them focused; describe what changes for the user.
- Anything that can lose work (discard, checkout, reset, amend…) must ask first
  and needs a test showing what it leaves untouched.
- By contributing you agree that your work is dual licensed under MIT OR
  Apache-2.0, like the rest of the project.
