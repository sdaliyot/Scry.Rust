## What this changes and why

<!-- One or two sentences: the problem this solves, not only what changed. -->

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
      and `cargo test --workspace` pass in `scry/` (or note what wasn't run, and why)
- [ ] Docs updated if documented behavior changed (`README.md`, `docs/development.md`,
      `docs/threat-model.md`, `skills/scry-rs/SKILL.md`, as they apply)
- [ ] Tests added or updated for the change

---
The maintainer reviews and merges every pull request. Contributions are welcome, but passing CI
doesn't merge a pull request by itself.
