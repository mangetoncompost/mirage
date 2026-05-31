# Contributing

Contributions are welcome. This document covers the basics.

## Reporting a bug

Open an issue on GitHub. Include:

- Your OS and architecture
- The Mirage version (or commit hash)
- What you did, what you expected, what happened instead
- Relevant log output (`MIRAGE_NO_UI=1 RUST_LOG=debug ./mirage`)

## Proposing a change

For small fixes (typos, minor bugs), open a PR directly.

For larger changes (new features, architecture changes), open an issue first
to discuss the approach before writing code.

## Submitting a pull request

1. Fork the repository and create a branch from `master`.
2. Make your changes. Keep commits focused and the history clean.
3. Ensure the CI passes locally before pushing:

```
cargo build --locked
cargo test --locked
cargo clippy --locked -- -D warnings
cargo fmt --check
```

4. Open a pull request against `master` with a clear description of what
   changes and why.

## What is welcome

- Bug fixes
- Support for new torrent client profiles
- Improvements to the live dashboard (new tabs, better layout)
- Linux quality-of-life improvements
- Performance improvements with no behaviour change

## What needs discussion first

- Changes to the announce protocol logic or the upload curve model
- New dependencies
- Breaking changes to the config file format

## Code style

The codebase uses `rustfmt` defaults and passes `clippy` with `-D warnings`.
Run `cargo fmt` before committing.

## Releasing

Releases are driven by git tags. The tag is the single source of truth for the
version: pushing a `vX.Y.Z` tag triggers the release workflow, which rewrites
`Cargo.toml` from the tag, builds the binaries for all platforms, creates the
GitHub Release, and publishes the crate to crates.io as `mirage-tui`.

To cut a release:

```
git tag vX.Y.Z
git push origin vX.Y.Z
```

There is no need to edit the `version` field in `Cargo.toml` by hand; the
workflow sets it from the tag (see `scripts/set_version.sh`). Use semantic
versioning, and never reuse a version already published on crates.io.
