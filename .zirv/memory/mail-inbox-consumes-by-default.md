## Memory
- Key: mail-inbox-consumes-by-default
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: mail, cli
- Paths: src/commands/ctx/mail.rs

zirv ctx inbox consumes the caller-visible mail it displays by default. --peek is the old broad, idempotent read (it also shows mail addressed to other sessions) and --consume is a no-op alias kept only for backward compatibility. mail::store appends a collision-free _NNN suffix on a same-second filename collision, because now_secs() has one-second granularity and two real sends that close together is common, not an edge case.
