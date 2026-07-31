#!/bin/sh
# Minimal interactive stand-in for an agent TUI, driven by wrap's PTY.
#
# Echoes every line back with a prefix so passthrough fidelity is checkable.
# Records injected slash-commands to $STUB_TUI_LOG and appends a compaction
# event to $STUB_TUI_TRANSCRIPT when it sees /compact, which is what wrap
# watches for when it verifies an injection. Exits on /exit, /quit or EOF.
set -eu
printf 'stub-tui ready\n'
while IFS= read -r line; do
  printf 'echo: %s\n' "$line"
  case "$line" in
    /compact*)
      [ -z "${STUB_TUI_LOG:-}" ] || printf '%s\n' "$line" >> "$STUB_TUI_LOG"
      if [ -n "${STUB_TUI_TRANSCRIPT:-}" ]; then
        printf '{"type":"system","subtype":"compact_boundary","content":"Conversation compacted"}\n' >> "$STUB_TUI_TRANSCRIPT"
        printf '{"type":"user","message":{"content":"post-compaction"}}\n' >> "$STUB_TUI_TRANSCRIPT"
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"[zirv] fresh"}],"usage":{"input_tokens":9000}}}\n' >> "$STUB_TUI_TRANSCRIPT"
      fi
      printf 'compacted\n'
      ;;
    /exit|/quit) printf 'bye\n'; exit 0 ;;
    /fail) exit 5 ;;
  esac
done
