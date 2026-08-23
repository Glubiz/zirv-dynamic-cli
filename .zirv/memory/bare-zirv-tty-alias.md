## Memory
- Key: bare-zirv-tty-alias
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: cli, dispatch, gotcha
- Paths: src/main.rs, src/utils.rs

Bare `zirv` (no args) starts `zirv ctx chat` only when the cwd holds a LOCAL .zirv directory and BOTH stdin and stdout are real terminals; otherwise it prints `zirv help`. A global ~/.zirv alone does not count, and `zirv | less` must never open a chat into the pipe. `zirv chat`/`zirv agent` are raw-argv aliases matched in main.rs before clap and reserved in utils::RESERVED_COMMANDS. An agent workflow that pipes zirv must call `zirv ctx chat` (or a verb) explicitly.
