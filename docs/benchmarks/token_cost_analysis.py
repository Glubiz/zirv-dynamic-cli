#!/usr/bin/env python3
"""
Token-cost measurement script for zirv issue #155 acceptance criteria.

Method (mirrors src/commands/ctx/window.rs `session_spend`/`sum_file` and
src/commands/ctx/event.rs `TranscriptUsage`, so these numbers use the exact
same "session" and "token class" definitions zirv's own `zirv ctx usage
--sessions` command uses):

  * A "session" is one *.jsonl transcript file, found by walking the given
    project directories RECURSIVELY (this includes each `subagents/*.jsonl`
    file as its own session, exactly like `window::session_spend` does --
    subagent turns spend the account's budget too and are not folded into
    the parent's numbers).
  * Within a file, every row with `"type":"assistant"` and a `message.usage`
    object contributes: `input_tokens`, `cache_creation_input_tokens`,
    `cache_read_input_tokens`, `output_tokens` (each summed across every
    such row in the file -- usage is per-API-call, not cumulative, so this
    matches `adapters::claude::fold_assistant_usage`'s summation).
  * "Fresh input" = input_tokens + cache_creation_input_tokens (the two
    classes that are NOT served from cache).
  * cache-hit ratio = cache_read_input_tokens / context_total(), where
    context_total = input_tokens + cache_creation_input_tokens +
    cache_read_input_tokens (TranscriptUsage::context_total(), and the same
    formula docs/benchmarks/token-cost.md section 1.2 documents for
    TranscriptUsage sources).
  * A session's date = the latest `timestamp` row parsed from the file, or
    the file's mtime if no row has a parseable timestamp.
  * Sessions are bucketed pre-epoch (date < 2026-08-27T00:00:00Z, the date
    phase 1 of the plan shipped in v2.31.0) vs post-epoch (date >=
    2026-08-27T00:00:00Z).

No number in this script's output is estimated or extrapolated -- every
figure is a direct sum/count/median over the real transcript files present
on this machine at run time. Only sessions/files that produced at least one
usage-bearing assistant row are counted (an empty or usage-less transcript
contributes nothing, never a manufactured zero).

See docs/benchmarks/token-cost.md section 4.1 for the full write-up,
caveats, and how to reproduce. Raw output from the run this document's
numbers were taken from is committed alongside this script as
token_cost_report.json.

Issue #225 measurement closeout: `--first-turn` switches to a second,
prefix-scoped mode (`first_turn_usage`/`run_first_turn_report`) that reports
only each session's FIRST usage-bearing assistant row instead of the whole
file's summed usage -- that row's `cache_creation_input_tokens` is the real,
tokenizer-accurate cost of ingesting the session's prompt prefix for the
first time (section 6 of token-cost.md), which the whole-file summary above
cannot isolate. See docs/benchmarks/token-cost.md section 6.6.
"""

import json
import math
import os
import statistics
import sys
from datetime import datetime, timezone

EPOCH = datetime(2026, 8, 27, 0, 0, 0, tzinfo=timezone.utc)

PROJECT_DIRS = [
    r"C:\Users\josj\.claude\projects\D--GitHub-zirv-dynamic-cli",
]
# Sibling project dirs for other zirv worktrees (e.g. zirv-perms), included
# if present on this machine.
PROJECTS_ROOT = r"C:\Users\josj\.claude\projects"


def find_sibling_zirv_dirs():
    siblings = []
    try:
        for name in os.listdir(PROJECTS_ROOT):
            full = os.path.join(PROJECTS_ROOT, name)
            if not os.path.isdir(full):
                continue
            lower = name.lower()
            if "zirv-perms" in lower or "zirv-dynamic-cli" in lower:
                if full not in PROJECT_DIRS:
                    siblings.append(full)
    except FileNotFoundError:
        pass
    return siblings


def parse_iso8601(value):
    try:
        # Python's fromisoformat before 3.11 doesn't accept trailing 'Z'.
        if value.endswith("Z"):
            value = value[:-1] + "+00:00"
        return datetime.fromisoformat(value)
    except (ValueError, AttributeError):
        return None


def walk_jsonl(root):
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            if name.endswith(".jsonl"):
                yield os.path.join(dirpath, name)


def first_turn_usage(path):
    """Issue #225 measurement-closeout addition: unlike `analyze_file` (which
    sums usage across an entire session), this returns only the FIRST
    usage-bearing `"type":"assistant"` row's four raw classes. That row is
    the one turn whose `cache_creation_input_tokens` was paid to ingest the
    session's prompt prefix for the first time (everything after it either
    hits the cache or reflects turn-specific content) -- the real,
    tokenizer-accurate cost of "ingesting the prefix" that `docs/benchmarks/
    token-cost.md` section 6 needs and `zirv ctx compile --measure`'s
    `bytes / 4` column can only estimate. Returns None if the file has no
    usage-bearing assistant row at all (same convention as `analyze_file`)."""
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if row.get("type") != "assistant":
                    continue
                message = row.get("message") or {}
                usage = message.get("usage")
                if not usage:
                    continue
                ts = parse_iso8601(row.get("timestamp") or "")
                return {
                    "path": path,
                    "input_tokens": int(usage.get("input_tokens") or 0),
                    "cache_creation_input_tokens": int(
                        usage.get("cache_creation_input_tokens") or 0
                    ),
                    "cache_read_input_tokens": int(usage.get("cache_read_input_tokens") or 0),
                    "output_tokens": int(usage.get("output_tokens") or 0),
                    "date": ts.isoformat() if ts is not None else None,
                    "is_subagent": "subagents" in path.replace("\\", "/").lower().split("/"),
                }
    except OSError as error:
        print(f"skip {path}: {error}", file=sys.stderr)
        return None
    return None


def run_first_turn_report(roots):
    """`--first-turn` CLI mode: one row per session's first usage-bearing
    turn, split top-level vs subagent (mirrors `analyze_file`'s own
    population split), reporting median/p95 `cache_creation_input_tokens` --
    the real per-session prompt-prefix ingestion cost, not an estimate."""
    records = []
    for root in roots:
        if not os.path.isdir(root):
            print(f"# root does not exist, skipping: {root}", file=sys.stderr)
            continue
        for path in walk_jsonl(root):
            record = first_turn_usage(path)
            if record is not None:
                records.append(record)

    top_level = [r for r in records if not r["is_subagent"]]
    subagent = [r for r in records if r["is_subagent"]]

    def summarize_first_turn(rows):
        creation = [r["cache_creation_input_tokens"] for r in rows]
        return {
            "sessions": len(rows),
            "median_cache_creation_input_tokens": median_or_none(creation),
            "p95_cache_creation_input_tokens": p95_or_none(creation),
            "median_input_tokens": median_or_none([r["input_tokens"] for r in rows]),
        }

    report = {
        "mode": "first-turn",
        "generated_at": datetime.now(tz=timezone.utc).isoformat(),
        "roots_scanned": roots,
        "top_level_only": summarize_first_turn(top_level),
        "subagent_only": summarize_first_turn(subagent),
        "top_level_sessions": sorted(
            top_level, key=lambda r: r["date"] or "", reverse=True
        )[:20],
    }
    print(json.dumps(report, indent=2, default=str))


def analyze_file(path):
    """Returns a dict of per-session totals, or None if the file contributed
    no usage-bearing assistant rows."""
    input_tokens = 0
    cache_creation = 0
    cache_read = 0
    output_tokens = 0
    message_count = 0
    latest_ts = None

    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if row.get("type") != "assistant":
                    continue
                message = row.get("message") or {}
                usage = message.get("usage")
                if not usage:
                    continue
                input_tokens += int(usage.get("input_tokens") or 0)
                cache_creation += int(usage.get("cache_creation_input_tokens") or 0)
                cache_read += int(usage.get("cache_read_input_tokens") or 0)
                output_tokens += int(usage.get("output_tokens") or 0)
                message_count += 1
                ts = parse_iso8601(row.get("timestamp") or "")
                if ts is not None and (latest_ts is None or ts > latest_ts):
                    latest_ts = ts
    except OSError as error:
        print(f"skip {path}: {error}", file=sys.stderr)
        return None

    if message_count == 0:
        return None

    if latest_ts is None:
        try:
            mtime = os.path.getmtime(path)
            latest_ts = datetime.fromtimestamp(mtime, tz=timezone.utc)
        except OSError:
            return None

    fresh_input = input_tokens + cache_creation
    context_total = fresh_input + cache_read
    cache_hit_ratio = (cache_read / context_total) if context_total > 0 else None

    return {
        "path": path,
        "input_tokens": input_tokens,
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cache_read,
        "output_tokens": output_tokens,
        "fresh_input": fresh_input,
        "context_total": context_total,
        "cache_hit_ratio": cache_hit_ratio,
        "message_count": message_count,
        "date": latest_ts.isoformat(),
        "epoch_bucket": "post" if latest_ts >= EPOCH else "pre",
        "is_subagent": "subagents" in path.replace("\\", "/").lower().split("/"),
    }


def median_or_none(values):
    return statistics.median(values) if values else None


def p95_or_none(values):
    """Nearest-rank p95: index ceil(0.95*n) - 1 of the sorted values (1-indexed
    rank ceil(0.95*n), converted to a 0-indexed slot). Plain and reproducible
    by hand, no interpolation-method ambiguity."""
    if not values:
        return None
    ordered = sorted(values)
    n = len(ordered)
    rank = max(1, math.ceil(0.95 * n))
    return ordered[rank - 1]


def summarize(sessions):
    n = len(sessions)
    total_output = sum(s["output_tokens"] for s in sessions)
    total_fresh_input = sum(s["fresh_input"] for s in sessions)
    total_cache_read = sum(s["cache_read_input_tokens"] for s in sessions)
    total_context = total_fresh_input + total_cache_read
    combined_cache_hit = (total_cache_read / total_context) if total_context > 0 else None

    ratios = [s["cache_hit_ratio"] for s in sessions if s["cache_hit_ratio"] is not None]

    subagent_count = sum(1 for s in sessions if s["is_subagent"])

    return {
        "sessions": n,
        "top_level_sessions": n - subagent_count,
        "subagent_sessions": subagent_count,
        "total_output_tokens": total_output,
        "total_fresh_input_tokens": total_fresh_input,
        "total_cache_read_tokens": total_cache_read,
        "combined_cache_hit_ratio": combined_cache_hit,
        "median_output_tokens_per_session": median_or_none(
            [s["output_tokens"] for s in sessions]
        ),
        "median_fresh_input_tokens_per_session": median_or_none(
            [s["fresh_input"] for s in sessions]
        ),
        "median_cache_read_tokens_per_session": median_or_none(
            [s["cache_read_input_tokens"] for s in sessions]
        ),
        "median_context_total_per_session": median_or_none(
            [s["context_total"] for s in sessions]
        ),
        "median_cache_hit_ratio_per_session": median_or_none(ratios),
        "median_message_count_per_session": median_or_none(
            [s["message_count"] for s in sessions]
        ),
        "p95_output_tokens_per_session": p95_or_none(
            [s["output_tokens"] for s in sessions]
        ),
        "p95_fresh_input_tokens_per_session": p95_or_none(
            [s["fresh_input"] for s in sessions]
        ),
        "p95_cache_read_tokens_per_session": p95_or_none(
            [s["cache_read_input_tokens"] for s in sessions]
        ),
        "p95_context_total_per_session": p95_or_none(
            [s["context_total"] for s in sessions]
        ),
    }


def main():
    roots = list(PROJECT_DIRS)
    siblings = find_sibling_zirv_dirs()
    for s in siblings:
        if s not in roots:
            roots.append(s)

    print(f"# scanning roots: {roots}", file=sys.stderr)

    if "--first-turn" in sys.argv[1:]:
        run_first_turn_report(roots)
        return

    sessions = []
    files_seen = 0
    for root in roots:
        if not os.path.isdir(root):
            print(f"# root does not exist, skipping: {root}", file=sys.stderr)
            continue
        for path in walk_jsonl(root):
            files_seen += 1
            result = analyze_file(path)
            if result is not None:
                sessions.append(result)

    print(f"# files_seen={files_seen} usage_bearing_sessions={len(sessions)}", file=sys.stderr)

    pre = [s for s in sessions if s["epoch_bucket"] == "pre"]
    post = [s for s in sessions if s["epoch_bucket"] == "post"]

    top_level = [s for s in sessions if not s["is_subagent"]]
    top_pre = [s for s in top_level if s["epoch_bucket"] == "pre"]
    top_post = [s for s in top_level if s["epoch_bucket"] == "post"]

    subagent_only = [s for s in sessions if s["is_subagent"]]
    sub_pre = [s for s in subagent_only if s["epoch_bucket"] == "pre"]
    sub_post = [s for s in subagent_only if s["epoch_bucket"] == "post"]

    report = {
        "generated_at": datetime.now(tz=timezone.utc).isoformat(),
        "epoch_cutoff": EPOCH.isoformat(),
        "roots_scanned": roots,
        "files_seen": files_seen,
        "usage_bearing_sessions": len(sessions),
        "pre_epoch": summarize(pre),
        "post_epoch": summarize(post),
        "combined": summarize(sessions),
        "top_level_only": {
            "note": "excludes subagents/*.jsonl -- one row per user-launched "
            "session only, a less confounded view than the all-sessions "
            "buckets above (which mix in a recent burst of short "
            "subagent-forked sessions)",
            "pre_epoch": summarize(top_pre),
            "post_epoch": summarize(top_post),
        },
        "subagent_only": {
            "note": "subagents/*.jsonl only -- the dominant session type in "
            "both buckets (489/538 pre, 108/112 post), so the fairest "
            "same-population before/after comparison this dataset supports",
            "pre_epoch": summarize(sub_pre),
            "post_epoch": summarize(sub_post),
        },
    }

    print(json.dumps(report, indent=2, default=str))


if __name__ == "__main__":
    main()
