## Memory
- Key: policy-and-safety-narrowing-folds
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: policy, safety, trust, config
- Paths: src/commands/ctx/policy.rs, src/commands/ctx/safety.rs, src/commands/ctx/config.rs

[policy] resolves through a narrowing fold (policy::resolve, Stance::max), so a repo layer may only tighten a capability, never widen it. [safety] folds deny/ask as a UNION while allow/default are REPO_FORBIDDEN (issue #83): a repo may add denials, never remove one. Both tables are lifted out of the layers before the ordinary deep merge -- the same seam sandbox.extra_deny and narrow_pace_* use. `zirv ctx safety check` is also the wired claude PreToolUse hook.
