# Contributing to zirv

Follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report vulnerabilities through
our [security policy](SECURITY.md).

## Reporting bugs and requesting features

Use the [GitHub issue forms](https://github.com/Glubiz/zirv-cli/issues/new/choose),
or file an issue from a terminal:

```sh
zirv report bug "<title>" --body "<details>"
zirv report feature "<title>" --body "<details>"
```

Include enough detail to reproduce a bug or explain the problem a feature solves.
Never include secrets or tokens in reports.

## Development setup

Install the stable Rust toolchain, then run from your checkout:

```sh
cargo install cargo-nextest
cargo build
```

For CLI installation and usage, see the [README](README.md).

## Making changes

Branch from `main`; never commit directly to `main`. Keep diffs focused and mirror
existing naming, structure, and style. Tests stay inline in `#[cfg(test)] mod tests`;
`tests/fixtures/` is data only.

Size verification to the change using [CLAUDE.md](CLAUDE.md#build-and-verify-by-tier):

- Docs or comments: run `cargo fmt -- --check` if Rust was touched; otherwise no checks.
- Code: run `cargo build`, `cargo nextest run <filter>` for touched modules, and
  `cargo clippy --all-targets -- -D warnings`.
- Before opening or updating a PR: run all five gates below once.

## Before opening a PR

```sh
cargo build
cargo nextest run --no-fail-fast
cargo test --verbose -- --test-threads=1
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Every PR must bump the version in `Cargo.toml` above its base version on `main`,
including docs-only PRs. CD releases on every merge to `main`, so an unbumped PR
duplicates a release tag. Run `cargo build` to update `Cargo.lock` and include it.

Commit subjects use `type(scope): summary`, with types such as `feat`, `fix`,
`docs`, and `chore`; for example, `fix(dash): restore keyboard focus`.
Do not include `Co-Authored-By` or `Generated with Claude Code` lines in commits.

## Pull requests

Open a PR targeting `main` with one logical change and explain what changed and
why. CI runs on pull requests to `main` and is the merge gate.
