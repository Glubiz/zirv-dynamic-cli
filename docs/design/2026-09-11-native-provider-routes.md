# Native provider routes, credentials, and account identities (issue #471)

**Date:** 2026-09-11 · **Issue:** #471 (step N02 of the native-runtime roadmap, #469) · **Status:** decided; no model transport or agent loop

## 1. Context

N01 separated a session's execution backend, logical role, provider route,
model identity, session identity, and UI surface. It deliberately left the
provider-route axis as a string because zirv did not yet own provider
accounts, endpoints, credentials, or an evidence model for native access.

Harness identity cannot fill that gap. A Claude Code or Codex login is a
subscription entitlement to that harness, not an API key, and a vendor name
alone cannot distinguish two accounts, two endpoints, or two independent
quota pools. Catalogue membership also proves only that zirv recognizes a
model name; it does not prove an account may use it.

This step defines those identities and proofs. It performs no generation
request and adds no agent loop. Existing harness endpoint overrides and
subscription usage windows remain unchanged.

## 2. Decision

### 2.1 Independent identity axes

The provider layer uses validated lower-case slug newtypes for `ProviderId`,
`EndpointId`, `AccountId`, `BillingPoolId`, and `RouteId`. A route resolves an
account, endpoint, protocol, billing class, and exact `ModelId { vendor, id }`.
The model id is an exact catalogue id after resolving a case-insensitive exact
id or alias; it is never the result of choosing the strongest fuzzy match. A
`vendor/` prefix is accepted only when it names the endpoint vendor. Version
and provider decorations such as `@date` and `:suffix` are never stripped.
Exact identity matters because N12/N13 cloud routes must send decorated model
ids verbatim rather than silently substituting another identity.

`BillingPoolId` is explicitly separate from account and provider. Its default
is the account id, while two accounts may deliberately name the same pool when
they truly share quota. Two routes using one account therefore remain one
pool; two accounts at one vendor default to two pools.

The static provider registry records protocol, catalogue vendor, base URL,
credential environment names, authentication shape, model-list path,
entitlement warning, and whether support is native or assigned to a later
roadmap step. Anthropic, OpenAI Responses, Google Generative AI, and generic
OpenAI-compatible endpoints are native registry entries. Vertex and Bedrock
are declared but route use is refused with their N12/N13 tracking issue.

### 2.2 Separate, opt-in `native.toml`

Native routing is enabled only when `~/.zirv/native.toml` exists. Schema 1
contains `[endpoint]`, `[account]`, `[route]`, `[roles]`, and
`[policy].allowed_routes`. Providers with a default URL contribute an implicit
endpoint named after the provider. OpenAI-compatible endpoints must declare a
base URL and catalogue-vendor slug; other providers fix their vendor and
forbid an override.

This is a separate file because released `ctx.toml` readers deny unknown
fields. Adding native keys there would make an older binary reject the
operator's existing configuration even though that binary has no native
runtime.

The repository layer `<repo>/.zirv/native.toml` may contain only `schema` and
`policy.allowed_routes`. The effective set is the intersection of operator
and repository sets, so a checkout can narrow route access but never add an
account, endpoint, credential, role binding, route, or allowed route. A role
left bound outside the intersection is an error rather than a fallback.

All references and provider/endpoint agreement are validated while loading,
before credentials or network access. Every route must declare a model that is
non-empty after trimming, including routes for rungless compatible vendors.
Schema versions newer than 1 fail with an upgrade instruction.

### 2.3 Credential references and protected stores

Accounts refer to credentials as `env:NAME`, `store:<item>`, or `file:<path>`.
Every account except `openai-compatible` must declare a credential reference;
compatible endpoints may deliberately operate without authentication.
Environment values are read through an injectable lookup. Files expand `~`,
must be regular files, and on Unix must not be group/world readable. Values
are trimmed, wrapped in `Secret`, and can be exposed only by the explicit
transport-facing accessor; debug and display always render `[redacted]`.

Harness login locations are denylisted. The Claude Code credential keychain
service and the Claude/Codex login files are refused with the explanation that
harness login tokens are subscription entitlements, not API credentials.
File paths are checked both lexically and after canonicalization, and store
names and path components are compared without case sensitivity. This
denylist is an operator guardrail against accidentally reusing a harness
login, not a security boundary against a hostile operator, who already owns
`~/.zirv/native.toml`. Credential storage applies the same refusal before
reading a secret or touching the OS store.
Claude.ai and ChatGPT subscription-billed accounts stop at `Configured`; the
harness backend remains the path that can spend those subscriptions.

`store:` uses the current user's protected OS store:

- macOS uses `security` generic passwords under service `zirv-native`.
- Linux uses Secret Service through `secret-tool`.
- Windows uses CurrentUser DPAPI files below the user's local application-data
  directory.

Every store call has a hard timeout using a spawn/try-wait/kill pattern, and
reads are fresh rather than cached. Linux and Windows writes carry the secret
over stdin. On macOS an interactive terminal prefers `security`'s own hidden
`-w` prompt. In a non-interactive invocation Apple's CLI requires the value as
the `-w` argv element; this is the one path where a secret crosses argv, and
it is a local process started by the operator's explicit credential-set verb.

### 2.4 State ladder and access matrix

Routes advance monotonically through `Recognized`, `Configured`,
`Credentialed`, `Reachable`, `Authenticated`, and `Validated`.

- Recognized means only that the model resolved against the endpoint vendor's
  catalogue. A rungless local or unknown compatible vendor accepts the
  operator's exact id and says it is outside the catalogue.
- Configured means account, endpoint, provider, model, and policy references
  agree.
- Credentialed means a non-empty, unexpired API credential resolved without
  network access.
- Reachable means an opt-in live model-list probe received HTTP.
- Authenticated means a credential was accepted by a probe that received HTTP
  200. A credential-less compatible endpoint can reach only `Reachable`.
- Validated requires a real model transport to verify behavior. N02 cannot
  produce it and reports that fact instead of inferring access from a list.

The inventory groups accounts by billing pool and emits one access row for
every configured role plus zirv's orchestrator, sub-orchestrator, worker,
reviewer, and distiller roles. State text makes pre-authentication states
visibly weaker than proven access.

### 2.5 Capability evidence

Capabilities are tri-state: unknown, declared, or verified. N02 contains a
small protocol/model-prefix table based on dated vendor documentation, but it
produces only unknown and declared values. Context windows reuse the existing
catalogue when known. No declaration is upgraded to verified by a catalogue
match or model-list response.

### 2.6 Optional live probe

`zirv ctx provider check --live` makes a ten-second-bounded GET request to the
provider's model-list endpoint with its configured auth scheme. The default
check and all list operations remain offline. The probe parses only model ids
and never emits bodies or headers. A 200 with a credential proves
authentication; without a credential it proves only reachability. A 401/403
proves reachability but records credential rejection or the need to declare a
credential, and a compatible endpoint's 404 is a reachable endpoint without
model listing. A missing route model is a note because vendor lists can
reflect entitlement restrictions or aliases; it is not silently substituted.

A credential is never attached to plaintext HTTP on a non-loopback host. Such
a live probe is skipped with an actionable problem, while the offline
inventory flags the endpoint. Plaintext loopback endpoints remain available
for local runtimes, and a credential-less compatible endpoint may still be
probed because there is no secret to disclose.

## 3. Consequences

- Native provider configuration is explicit and has no effect on the harness
  backend.
- Route reports and JSON inventories contain references and state, never
  resolved secrets.
- Repository policy can remove native choices but cannot cause zirv to spend
  a different account or endpoint.
- Exact model identity, account identity, and quota identity can evolve
  independently in later runtime steps.
- `SessionSpec.provider_route` is now the transparent `RouteId` newtype, while
  protocol-v1 JSON remains wire-compatible.

## 4. Deferred

- N07/N08 add Anthropic/OpenAI generation transports and are the first steps
  allowed to record verified capabilities or make `Validated` reachable.
- N12/N13 implement Google/Vertex and compatible/Bedrock transport details.
- N18 keys usage, spend, and health by `BillingPoolId`. Today's vendor-slug
  usage window is the degenerate case where pool equals vendor; this step does
  not touch `usage.rs`, `health.rs`, or `window.rs`.
- OAuth and cloud refresh flows are not implemented. Native API-key providers
  need none in N02; store and file reads are fresh on every resolution, with
  no cache on which concurrent callers could race.
- The agent loop, tool execution, journals, and native conversation UI remain
  owned by their later roadmap steps.

## 5. Downgrade path

An older zirv binary ignores `native.toml` entirely because it reads only
`ctx.toml`; `ctx.toml` is untouched. Downgrading therefore requires deleting
nothing. The separate file remains inert until a native-aware binary is used
again.
