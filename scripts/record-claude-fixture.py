#!/usr/bin/env python3
"""Copy + scrub a real claude transcript into tests/fixtures/.

Usage: python3 scripts/record-claude-fixture.py <source.jsonl>

Keeps a window around the first compact_boundary so the fixture exercises
compaction, tool errors and token usage. Rewrites identifying paths, redacts
credential-shaped strings, truncates long tool output, and pins the session
uuid. Also writes the expectations file the Rust parser test asserts against.

Sidechain rows are filtered by the parser but are not guaranteed to appear in
any given window, so sidechain coverage lives in a synthetic test instead.
"""
import json
import pathlib
import re
import sys

BEFORE, AFTER = 70, 110
FIXTURE_UUID = "00000000-0000-4000-8000-000000000001"
# The sk- and gh- alternatives are word-anchored on purpose: without it,
# "task-notification" matches sk-<8 or more> and is corrupted to "taREDACTED".
# Keep this list and the guard test in
# src/commands/ctx/adapters/claude.rs (recorded_fixture_carries_no_personal_data)
# in step: the guard is what proves the scrub actually held.
SECRET = re.compile(
    r"("
    r"\bsk-[A-Za-z0-9_\-]{8,}"
    r"|\bgh[pousr]_[A-Za-z0-9]{8,}"
    r"|\bAKIA[0-9A-Z]{12,}"
    r"|-----BEGIN [A-Z ]*PRIVATE KEY-----"
    r"|ApiKey\s+\S+"
    r"|Bearer\s+\S+"
    r"|\beyJ[A-Za-z0-9_\-]{10,}"
    r"|\b[Kk]ey=[A-Fa-f0-9]{8,}"
    r"|\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+\.[A-Za-z]{2,}"
    r")"
)
ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "tests" / "fixtures" / "claude-real-session.jsonl"
EXPECTED = ROOT / "tests" / "fixtures" / "claude-real-session.expected.json"


def scrub(node):
    if isinstance(node, dict):
        return {k: scrub(v) for k, v in node.items()}
    if isinstance(node, list):
        return [scrub(v) for v in node]
    if isinstance(node, str):
        text = node.replace("/Users/jonathansolskov", "/home/testuser")
        text = text.replace("jonathansolskov", "testuser")
        text = SECRET.sub("REDACTED", text)
        return text[:200] + ("..." if len(text) > 200 else "")
    return node


def tokens(usage):
    keys = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")
    return sum(int(usage.get(k) or 0) for k in keys)


def main():
    src = pathlib.Path(sys.argv[1])
    rows = [json.loads(line) for line in src.read_text().splitlines() if line.strip()]

    boundary = next(
        (i for i, r in enumerate(rows) if r.get("subtype") == "compact_boundary"), None
    )
    if boundary is None:
        sys.exit("source has no compact_boundary; pick another transcript")

    window = rows[max(0, boundary - BEFORE) : boundary + AFTER]
    kept = []
    for row in window:
        row = scrub(row)
        for key in ("sessionId", "session_id"):
            if key in row:
                row[key] = FIXTURE_UUID
        kept.append(row)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("".join(json.dumps(r, separators=(",", ":")) + "\n" for r in kept))

    exp = dict.fromkeys(
        (
            "turn_start",
            "assistant",
            "tool_call",
            "tool_result_error",
            "tool_result_ok",
            "compaction",
        ),
        0,
    )
    last_tokens = 0
    files = set()
    for row in kept:
        if row.get("isSidechain") is True:
            continue
        kind = row.get("type")
        msg = row.get("message") or {}
        content = msg.get("content")
        if kind == "user":
            if row.get("isMeta") is True:
                continue
            results = [
                b
                for b in (content or [])
                if isinstance(b, dict) and b.get("type") == "tool_result"
            ]
            if not results:
                exp["turn_start"] += 1
            for block in results:
                key = "tool_result_error" if block.get("is_error") else "tool_result_ok"
                exp[key] += 1
        elif kind == "assistant":
            exp["assistant"] += 1
            last_tokens = tokens(msg.get("usage") or {})
            for block in content or []:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    exp["tool_call"] += 1
                    raw = block.get("input")
                    if isinstance(raw, dict):
                        for k in ("file_path", "notebook_path", "path"):
                            if isinstance(raw.get(k), str):
                                files.add(raw[k])
        elif kind == "system" and row.get("subtype") == "compact_boundary":
            exp["compaction"] += 1

    exp["last_context_tokens"] = last_tokens
    exp["files_touched_min"] = len(files)
    EXPECTED.write_text(json.dumps(exp, indent=2, sort_keys=True) + "\n")

    print(f"wrote {OUT} ({len(kept)} lines)")
    print(EXPECTED.read_text())


if __name__ == "__main__":
    main()
