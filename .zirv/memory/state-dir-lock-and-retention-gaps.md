## Memory
- Key: state-dir-lock-and-retention-gaps
- Written-by: claude
- Written: 1788980047
- Verified: 1788980047
- Source: explicit

Two audited state-dir gaps with no GitHub issue and no source comment: (1) objective.rs store/roll_up_spend do a read-modify-write with no interprocess lock (group.rs and memory.rs use BankLock), so two zirv ctx loop processes on one repo slug rolling up spend concurrently can lose a delta; (2) codex.rs's per-session rollout pin files under <state>/rollouts/ are never pruned, even on clean exit, so the directory grows without bound.
