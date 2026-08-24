## Memory
- Key: script-resolution-order
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: dispatch, scripts, cli
- Paths: src/main.rs, src/input.rs, src/utils.rs

Dispatch order: raw-argv intercepts first (ctx, chat, agent, skill, workflow, test, verify, artifact, memory, context, setup, top-level --help), then clap's Input::parse (help/version/init/create), then Input::get_file_path -- literal path, .zirv/<name>.{yaml,json,toml}, .zirv/.shortcuts.yaml, ~/.zirv/<name>.ext, ~/.zirv/.shortcuts.yaml. So .zirv/ctx.yaml is permanently unreachable, and RESERVED_ZIRV_FILES keeps ctx.toml/.settings.toml/verify.toml/.shortcuts.yaml out of script lookup.
