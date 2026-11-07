# Contributing

## Building and testing

[`docs/development.md`](docs/development.md) describes the layout, the build and the tests. In
short, from the `scry` folder:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

CI runs these on Windows, Linux and macOS, and builds the demos with `cargo scry`.

## Pull requests

This project has one maintainer, who reviews and merges every pull request, including their own.
So:

- Open an issue first for anything nontrivial, so the approach can be agreed on before you spend
  time on it.
- Keep each pull request to one change with one reason. A feature mixed with an unrelated cleanup
  will be asked to split.
- Follow the existing code's conventions and comment density. `docs/development.md` explains the
  architecture and the reasons behind the less obvious decisions.
- Update the docs when behavior changes: `README.md`, `docs/development.md`,
  `docs/threat-model.md` and `skills/scry-rs/SKILL.md`, as they apply.

## Bug reports

Please use the bug report template. It asks for what's needed to reproduce a problem: the version,
the platform, the exact command, and what you expected and got. An issue that only says "not
working" will be closed with a request to resubmit it with those details.

## Security issues

Please don't open a public issue for a suspected vulnerability. See [`SECURITY.md`](SECURITY.md).
