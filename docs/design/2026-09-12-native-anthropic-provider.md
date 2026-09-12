# Native Anthropic Messages provider (N07)

**Date:** 2026-09-12 · **Issue:** #476 · **Roadmap:** #469

## Boundary

`provider::adapter::ProviderAdapter` transports one provider-neutral request.
It does not own the agent loop, execute tools, choose fallback routes, persist
messages, or invoke Claude Code. `AnthropicMessagesAdapter` resolves the N02
route/account/endpoint/model/credential identity, rejects subscription billing
before credential access, and calls `POST /v1/messages` directly.

The request exposes only Anthropic message content and each Zirv tool's name,
description, and JSON input schema. Permission claims, executor metadata, and
credentials never enter tool schemas or diagnostics. Non-loopback plaintext
endpoints are refused.

## Streaming and continuation

The incremental SSE accumulator accepts protocol events independently of HTTP
chunk boundaries. Tool JSON is buffered and parsed only when its block closes;
malformed or truncated JSON never becomes a completed tool call. Multiple tool
blocks and consecutive matching results retain their exact IDs.

Thinking text and its provider signature remain one typed block.
`redacted_thinking` and `stop_details` remain opaque provider data. These values
serialize for exact continuation replay while their `Debug` representation is
always redacted. The N03 journal stores signatures, redacted blocks, and the
separate reasoning-token usage class without projecting opaque contents into
portable summaries.

## Controls, failures, and usage

Requests retain exact model IDs. Unsupported manual/adaptive thinking,
interleaving, display, and effort combinations fail before transport. Prompt
caching supports disabled, five-minute ephemeral, and one-hour ephemeral
modes.

Responses normalize all documented stop reasons and preserve unknown future
ones. Failures carry a stable class, scope, HTTP status, provider request ID,
and retry hint. The classes distinguish cancellation, first-event and idle
timeouts, authentication, permission/entitlement, model access, invalid
configuration, context overflow, rate limits, overloads, provider/transport
errors, invalid streams, and invalid tool arguments. Usage keeps ordinary
input, output, cache creation, cache read, and reasoning tokens distinct.

## Verification

Frozen fixtures under `tests/fixtures/provider/anthropic/v1/` cover streamed
opaque thinking plus multiple tools and malformed tool JSON. Inline tests also
cover request encoding through a local direct HTTP server, continuation
relationships, cancellation, timeouts, status/error normalization, retry
hints, exact model controls, and diagnostic redaction.

The ignored `live_anthropic_messages_contract` test is opt-in and requires
`ANTHROPIC_API_KEY` plus an entitled exact model in
`ZIRV_ANTHROPIC_LIVE_MODEL`; ordinary CI never requires credentials.
