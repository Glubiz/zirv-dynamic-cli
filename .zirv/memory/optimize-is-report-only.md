## Memory
- Key: optimize-is-report-only
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: optimize, invariant, security
- Paths: src/commands/ctx/optimize.rs

zirv ctx optimize may read any configuration surface but writes only to stdout, its own timestamped report copy under the state dir, and an explicit --out path -- never an analysed file. A test snapshots the analysed tree before and after a run and asserts it is byte-identical. Its judgment model child's own tools are restricted too (see distiller-read-only-pins). Keep any new analysis inside those three write targets.
