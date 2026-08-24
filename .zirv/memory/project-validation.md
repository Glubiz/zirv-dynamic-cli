## Memory
- Key: project-validation
- Written-by: zirv-setup
- Written: 1787497926
- Verified: 1787497926
- Source: setup
- Importance: normal
- Confidence: high
- Tags: validation, commands
- Paths: Cargo.toml

Use the repository-defined validation commands that apply to the changed area:
- `cargo clippy --all-targets --all-features`
- `cargo fmt --check`
- `cargo test`
