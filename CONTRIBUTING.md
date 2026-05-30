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
