# Native execution enforcement

**Date:** 2026-09-11 · **Issue:** #473 · **Roadmap:** #469 N04

## Outcome

`src/commands/ctx/runtime/enforcement.rs` is the mandatory authorization
boundary for every native effect. Provider responses remain untrusted data;
N05 and later tool registries turn them into typed `ExecutionAction` values,
then execute only after the broker returns an `Authorization`.

The broker reuses the existing authorities instead of creating native-only
copies:

- `CtxConfig::{policy,safety}` supplies the canonical narrowing-only policy
  and command classifier. `ConfigPolicySource` reloads both at every effect.
- `seat.rs` supplies session, role, runtime and generation identity.
- `permit.rs` supplies the RAII writer permit. The permit now exposes only
  the worktree it covers, letting the broker verify exact ownership without
  exposing or duplicating permit state.
- `provider::credential` remains the only provider-secret resolver. Provider
  credentials are never placed in a tool environment.

## Authorization order

Every call fails closed in this order:

1. Verify the persisted seat still names this native session, role and exact
   generation, and is not parked.
2. Reload policy; any unreadable or unparsable policy layer refuses effects.
3. Resolve action paths through existing symlinks or junctions. Missing leaf
   components are appended only after the nearest existing ancestor is
   canonicalized.
4. Enforce read/write/artifact/network claims. Protected credential, state
   and git-administration roots take precedence over ordinary file tools.
5. Require a live writer permit covering the exact linked worktree for every
   declared repository or git-metadata mutation.
6. Apply the canonical capability stances and, for processes, the existing
   `SafetyPolicy` classifier.
7. If required, verify an operator approval signed by a process-local
   authority. The signature covers approver, issue/expiry time and an exact
   scope digest.
8. For a process, require a verified platform launcher and return the
   immutable sandbox policy consumed by N05. Missing containment is a typed
   `IsolationUnavailable` error, never an unsandboxed fallback.

Approval scope includes the full typed action/arguments, canonical resolved
paths, session, task, role, generation, resource-claim fingerprint and current
policy fingerprint. Changing any one invalidates reuse. Headless sessions
cannot satisfy `ask`; parent grants cannot authorize a child because the
child identity produces a different digest.

## Filesystem and git scope

Ordinary file tools read only declared roots and write only the assigned
worktree or explicitly operator-declared outside roots. State, standard
credential directories, provider credential files and git administration are
protected even when they appear below a broader root.

Linked-worktree git access is discovered with `git rev-parse
--path-format=absolute`. Process sandboxes receive write access only to that
worktree's private git directory plus the common object/ref/log stores needed
for normal commits. A main checkout is rejected because its git directory is
also the common directory; granting it wholesale would expose hooks, config
and every linked worktree. Structured file tools never write git metadata.

Authorization returns canonical paths. N05 must operate on those paths and
preserve its own expected-content/open-time checks; it must not reopen the
unresolved model spelling.

## Process and network containment

The platform launcher receives a scrubbed environment, read roots, exact
writable roots, masked credential/state roots and a binary network decision.
Host-scoped network allowlists are enforced by brokered HTTP tools only;
arbitrary processes are refused unless the operator granted unrestricted
process network access. Descendant cleanup remains separate from containment.

| Platform | Mechanism | Current claim |
|---|---|---|
| Linux | bubblewrap (`bwrap`), read-only host bind, exact writable rebinds, protected masks, namespace network isolation | Supported only when an executable `bwrap` is detected; otherwise explicit unavailable |
| macOS | Seatbelt via `/usr/bin/sandbox-exec`, default deny, protected-path denies, exact write allows, optional network allow | Supported only when the system launcher exists; otherwise explicit unavailable |
| Windows | Zirv restricted-token/AppContainer helper contract; Job Objects remain cleanup only | The broker contract is implemented, but native process execution remains unavailable until the Zirv helper is installed and its platform gate passes |

Zirv therefore makes no blanket cross-platform native-coding claim at N04.
Brokered effects are portable; arbitrary process support is a runtime
capability and must be shown as unavailable when its verified mechanism is
missing. N05 owns process lifecycle and the Windows helper implementation;
N22 owns packaging it. Neither may bypass this gate.

## Credential separation

The tool environment removes explicitly configured provider variables and
common token/secret/password/API-key/credential variables, SSH agent and
askpass channels, cloud profiles, and credential-config directory overrides.
Linux bubblewrap clears the environment before re-adding the scrubbed map.
macOS and Windows launchers receive only the scrubbed map. Provider transports
obtain credentials out of band from `provider::credential::resolve`.

## Verification evidence

Inline deterministic tests cover deny/ask/allow behavior, exact writer scope,
outside-root refusal, symlink escape, protected state, credential scrubbing,
stale-generation fencing, policy/action approval invalidation, fail-closed
isolation, Linux and Seatbelt profile construction, and linked-worktree git
scope. CI runs the complete broker suite on Linux, macOS and Windows in the
`Native Enforcement` matrix. The Windows run validates portable broker and
filesystem semantics and the honest unavailable result until the helper is
shipped; it does not mislabel a Job Object as sandbox evidence.
