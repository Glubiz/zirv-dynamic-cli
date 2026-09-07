## Summary

<!-- Explain what changed and why. Add Closes #<issue> if applicable. -->

## Checklist

- [ ] `Cargo.toml` version bumped above `main`.
- [ ] All five gates run locally and pass: build, nextest (`--no-fail-fast`), serial tests (`--test-threads=1`), fmt check, and clippy (`-D warnings`).
- [ ] Tests added or updated for behaviour changes.
- [ ] No `Co-Authored-By` or `Generated with Claude Code` lines in commits.
