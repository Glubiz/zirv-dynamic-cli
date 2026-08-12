---
last-verified: 2026-08-12
---

# Shortcuts

> [!tip] Quick Reference
> - `.shortcuts.yaml` maps a short key (e.g. `t`) to a target script file (e.g. `test.yaml`), living alongside the scripts it points to — local `.zirv/.shortcuts.yaml` or global `~/.zirv/.shortcuts.yaml`.
> - It is consulted only *after* a direct `<name>.<ext>` match fails in that same directory, and it's tried for the local directory before the global one falls back to the same two-step check.
> - `zirv create --shortcut <key>` registers one at script-creation time; it's otherwise a hand-edited YAML map.
> - Cross-links: the lookup order this file participates in is [[Script Resolution]]; the file structure the shortcut resolves *to* is [[Script Files]].

> [!warning] If changed
> Update [[Script Resolution]] if the fallback order changes, and [[Utilities]] if `candidate_names_in_dir`/`suggest_matches` (which read shortcut keys) change.

## Structure

```yaml
shortcuts:
  <key>: <script-filename-or-stem>
```

`utils::Shortcuts` deserializes this into a `HashMap<String, String>`. The mapped value is resolved two ways, in order (`input::find_script_in_dir`):

1. As a literal path relative to the directory (`dir.join(mapped_file)`).
2. With each supported extension appended in turn (`yaml`, `yml`, `json`, `toml`), so `tp: test-params` works the same as `tp: test-params.yaml`.

## When it's consulted

Only as a fallback. `find_script_in_dir` first tries `<name>.yaml`/`.yml`/`.json`/`.toml` directly; only if none of those exist does it read `.shortcuts.yaml` and look up `name` as a key. This happens once for the local `.zirv/` directory and, if nothing matched there, again for the global `~/.zirv/` directory. See [[Script Resolution]] for the full order.

## Resilience

A `.shortcuts.yaml` that fails to read or fails to parse does **not** abort the lookup. `find_script_in_dir` warns (`crate::output::warn`) and treats the shortcuts as absent for that lookup, falling through to whatever comes next — a direct extension match in the same directory still succeeds, and only a lookup that genuinely needed the shortcut ends in "not found". `create_script` treats the file the same way: recoverable, not fatal.

## Discoverability

- `zirv help` lists shortcuts separately from scripts, under "Available Shortcuts" (local) / "Global Shortcuts", as `key -> target`.
- Shortcut keys are included in `candidate_names_in_dir`, so a mistyped shortcut key can surface in a "did you mean" suggestion just like a script name (see [[Script Resolution]], [[Utilities]]).
- A shortcut key equal to a reserved built-in name (`help`, `h`, `version`, `v`, `init`, `i`, `create`, `c`, `ctx`) is unreachable — `main.rs` matches built-ins first — and `zirv help` marks it `(shadowed by a built-in command, unreachable)`.

## Creating one

`zirv create` (`c`) can register a shortcut interactively, or non-interactively via `--shortcut <key>` alongside `--name` and `--global`; an empty shortcut string means "no shortcut". The shortcut is appended into the target directory's `.shortcuts.yaml`.

## Example (this repo's `.zirv/.shortcuts.yaml`)

```yaml
shortcuts:
  tf: test-fail.yaml
  tp: test-params.yaml
  gc: commit.yaml
  tfp: test-fallback-proceed-on-failure.yaml
  tfb: test-fallback.yaml
  t: test.yaml
  tno: test-no_options.yaml
  tcc: test-concurrentcy.yaml
  tc: test-capture.yaml
  tof: test-on_failure.yaml
  ts: test-secret.yaml
  cl: claude.yaml
```

So `zirv t` runs `.zirv/test.yaml`, and `zirv gc "message"` runs `.zirv/commit.yaml` — which itself chains to `zirv t` as one of its own steps (see [[Script Files]] on script chaining).
