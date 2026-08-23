## Memory
- Key: commandtypes-hand-deserialized
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: script-runner, serde, design
- Paths: src/script_runner/command_types.rs, src/script_runner/script.rs

CommandTypes (Command / Commands / Agent) is deserialized BY HAND, dispatching on which key a step's mapping carries, rather than serde's untagged enum -- untagged silently picks the first variant that fits and reports only "data did not match any variant", which is useless for a user's YAML typo. Keep any new step kind on the hand-written path. A Commands (list-of-lists) step opens a new terminal window per OS instead of running inline.
