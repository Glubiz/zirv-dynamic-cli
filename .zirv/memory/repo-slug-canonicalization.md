## Memory
- Key: repo-slug-canonicalization
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: state, gotcha, migration
- Paths: src/commands/ctx/state.rs

state::repo_slug canonicalizes the path before slugging it (fixed 2026-08-21), so a symlinked checkout or macOS /var vs /private/var no longer splits one repository's memory, mail, handoffs, and workflow state across two slugs. Residual: state written under the OLD non-canonical slug is not moved, merged, or flagged -- every reader computes the new slug and simply does not find it, and there is no cleanup command.
