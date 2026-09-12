# Native context compiler and knowledge tools

**Date:** 2026-09-12 · **Issue:** #475 · **Roadmap:** #469 N06

## Outcome

`src/commands/ctx/runtime/context.rs` is the provider-neutral selection and
budget boundary for a native turn. Provider transports receive
`CompiledNativeContext`; they do not rediscover files, reinterpret source
trust, or invoke a harness adapter. The existing `ctx compile` and all legacy
harness prompt behavior remain unchanged.

Each provider message has an `instruction` or `data` role plus a source and
trust class. Zirv methodology, the automatically selected native role
methodology, the model/capability profile, built-in skills, and operator
instructions are authoritative instruction messages. Repository prompt files,
canonical common context, repository skills, and shared memory are always
untrusted data. Native compilation never reads harness-specific
`claude.md`/`codex.md` context or a vendor CLI instruction file.

## Deterministic selection

Sources are selected in this order:

1. Zirv engineering standard, native role methodology, and model profile.
2. Optional operator role instructions.
3. Repository-root through current-directory `.zirv/system-prompt.md`, then
   canonical `.zirv/context/common.md`, all bounded and untrusted.
4. The active workflow state and only that step's resolved skills. Built-in,
   operator-global, and repository skills retain distinct provenance/trust.
5. Session memory, then the existing deterministic private/global/shared core
   and query-ranked merge. Existing entry caps and precedence remain intact.
6. Explicit user constraints, pending runtime actions, the operator task, and
   opaque evidence handles with bounded summaries.

The ordered stable prefix ends before workflow, memory, task, and evidence.
Its SHA-256 digest and message count let provider adapters use caching without
pretending dynamic sources are stable. Every source records raw/delivered
bytes, token count, decision, budget reason, and an optional independent
source version. The `documentation` source kind and version field reserve the
provider-neutral seam #439 can populate without overloading ids or trust.

## Budget and evidence contract

The output reservation is removed from the model context window first. Tool
schemas are counted next. Provider tokenizers may supply exact message/tool
counts; any unavailable count uses the conservative three-bytes-per-token
fallback plus message overhead and marks accounting as estimated.
Models whose declared/verified profile lacks tool calling receive no tool
schemas; unknown capability is never treated as permission or availability.

All required-source tokens are reserved before any optional source is packed.
Optional text is included, prefix-truncated, or excluded with provenance. The
user task, active workflow/skills, and required evidence references are never
silently dropped: compilation fails closed when they cannot all fit. Evidence
is represented by a validated opaque handle and bounded summary; raw output
stays in the existing store and is retrieved by `output_read` ranges.

## Native knowledge surface

The closed native tool registry adds `memory_recall`, `memory_remember`,
`memory_forget`, and `context_search`. All requests are typed before the N04
broker sees their `Knowledge` action. Session is the safe memory-write default.
Shared writes require repository-write policy and the exact live writer lease;
state-local scopes do not acquire repository authority.

The implementation calls the existing memory/search services. Their bank
locks, atomic replacement, journals, deterministic scope precedence,
promotion/rollback behavior, and report-only optimizer remain the authorities;
N06 creates no second memory store and no model-backed search path.

## Deferred consumers

N07–N09 provider transports and the agent loop will map these typed messages
to vendor wire roles and execute the returned tool calls. N14 can add
version-aware documentation retrieval as another explicitly typed/provenanced
source without changing the trust or budgeting contract.
