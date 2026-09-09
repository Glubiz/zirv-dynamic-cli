## Memory
- Key: cost-ledger-never-gates-spawn
- Written-by: claude
- Written: 1788979282
- Verified: 1788979282
- Source: explicit

The cost ledger (issue #264) is additive to usage-window pacing, never coupled to it: pacing never reads a price and pricing never gates, refuses or throttles a spawn. Dollar-budget enforcement was an explicit non-goal of #264. Do not wire zirv ctx spend or price figures into any refusal or throttle path without revisiting that decision; price.rs documents its purity but not this separation from pace.rs.
