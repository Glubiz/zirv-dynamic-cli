# Ruflo evaluation — what's worth adopting into zirv (issue #240)

**Date:** 2026-08-31 · **Issue:** #240 · **Status:** research only, no implementation in this note

## 1. Context

Issue #240 asked for an evaluation of [ruvnet/ruflo](https://github.com/ruvnet/ruflo) — an
"AI swarm meta-harness" that has been picked up on social media as a reference point for
what a multi-agent orchestration tool "should" look like — to check whether any of its
ideas are worth adopting into zirv, and to reject the rest on the record rather than by
silence.

**What Ruflo is:** a TypeScript/Node pnpm monorepo (`v3/@claude-flow/{swarm,memory,security,
integration,performance,neural,cli,shared,deployment}`, plus `v3/mcp`), renamed from
`claude-flow` (the npm package and CLI are still `claude-flow`/`ruflo`). It markets itself as
a "queen-led swarm" that dispatches 100+ named agent personas across topologies with
Raft/Byzantine consensus, a neural memory layer, and a large MCP/plugin ecosystem, all wired
into Claude Code via an MCP server, plugin-marketplace markdown bundles, and CLI-dispatched
`settings.json` hooks. It has no supervising-process analog to zirv's PTY `wrap`/rot-scoring
loop — it is a toolset a harness calls into, not a process that watches a harness.

The repo is real and active: 70k stars, 8.4k forks, 877 open issues, created June 2025, last
push 2026-08-31, near-daily releases (v3.38.20 as of 2026-08-24). One genuinely interesting
piece — `aimds-*` Rust crates / the `aidefence` npm package — lives in a sibling repo,
`ruvnet/midstream`, and is consumed as a dependency rather than owned by Ruflo itself.

**Method:** every headline claim in Ruflo's own README and docs was checked against the
actual repository content — source files, internal doc pages, test suites, and package
manifests — rather than taken at face value. Where Ruflo's own internal documents disagree
with its README (this happens more than once), the disagreement is recorded as evidence
against the marketing claim, not resolved by picking a side. No claim below is taken from a
blog post, a tweet, or Ruflo's marketing copy alone.

## 2. Claim-by-claim verdict

| Area | Claim | Reality | Verdict |
|---|---|---|---|
| Swarm coordination | "100+ agents," queen-led swarm, Raft/Byzantine/gossip consensus, multiple topologies | The coordination substrate is real TypeScript with tests (`unified-coordinator.ts`, `queen-coordinator.ts`, `topology-manager.ts`, `consensus/{raft,byzantine,gossip}.ts`, each with `__tests__`). "100+ agents" is ~200 mostly-markdown `SKILL.md` persona files under `.agents/skills/`; Ruflo's own docs elsewhere cite "98 agents," not 100+. | PARTIAL — real substrate, persona count is inflation |
| Memory (AgentDB/HNSW, SONA, ReasoningBank) | Vector memory with HNSW search, a "SONA" neural layer, "ReasoningBank" learning | `@claude-flow/memory` has a real HNSW implementation with measured benchmarks (0.53ms/search, ~0.99 recall@10) and a `MemoryGraph` (PageRank + label propagation) over a sql.js+AgentDB hybrid backend — that part is real engineering. SONA/ReasoningBank is much thinner: mostly skill markdown plus an `sona-tools` MCP module, and its own `LearningBridge` code comment says it "degrades gracefully when unavailable." | PARTIAL, leaning REAL on the vector/graph core; SONA/ReasoningBank is thin glue |
| Integration (210+ MCP tools, 35 plugins, multi-provider, federation) | "210+ MCP tools across 5 groups," "35 plugins," cross-org federation | Internally inconsistent by Ruflo's own docs: `v3/mcp/tools/README.md` says the core server ships 13 MCP tools; the root README says ~210; a third doc says 313/314 aggregated. Plugin counts disagree across docs too (21/32/33+21/35). Plugins are manifest+markdown bundles, not a runtime ecosystem. Federation source files exist (`federation-hub.ts`, `federation-transport.ts`) but per-provider depth is unverified. | MOSTLY-MARKETING on the headline counts; real but modest core |
| Automation (27 hooks, 12 auto-triggered workers) | 27 lifecycle hooks, 12 background workers auto-triggered on events | The `hooks-automation` skill documents 18 hooks, not 27, implemented as thin CLI dispatch (`npx claude-flow hook <name>`) wired into `settings.json` — no dedicated hook engine. The "12 auto-triggered workers" figure matches the README and a real `workers/` directory exists. | PARTIAL |
| Security (AIDefence, CVE remediation, `ruflo verify`) | AI-specific threat detection, CVE tracking/remediation, build-provenance verification | The strongest area. AIDefence (`aimds-core/detection/analysis/response`) is real, tested Rust in the sibling `midstream` repo — 79/79 cargo tests, `unsafe_code = deny`, active RUSTSEC patching. `CVE-REMEDIATION.ts` is a tracking registry, but real fix modules sit next to it (`password-hasher.ts`, `path-validator.ts`, `safe-executor.ts`, `input-validator.ts`, `tool-output-guardrail.ts`, `mcp-composition-inspector.ts`). `ruflo verify` is a genuine build-provenance witness (SHA-256 + Ed25519 over release artifacts) — narrower than the marketing implies, but real and functioning. | REAL |

Net read: the numbers that make Ruflo sound enormous (100+ agents, 210+ tools, 35 plugins,
27 hooks) do not survive a check against Ruflo's own internal documentation, let alone the
code. What is real is a modest, competently-tested TypeScript coordination/memory core, plus
one genuinely strong Rust security component it doesn't own outright. "Swarm intelligence"
is orchestrated prompt-dispatch over that core, not a new coordination paradigm.

## 3. Rejected features

**Raft/Byzantine consensus for agent coordination.** This is coordination theater for LLM
agents: Raft and Byzantine fault tolerance exist to keep independently-failing, potentially
malicious distributed nodes agreeing on shared state under partition and crash faults. A
`zirv agent` delegation tree has none of those failure modes — a worker that goes bad is
detected by rot scoring, not by voting it out of a quorum. zirv's supervised delegation
(`zirv agent`, `zirv ctx dash`) plus the dashboard's own pane roster already cover the real
need here: know what's running, know if it's degrading, restart or hand off deterministically.
Consensus protocols would add real distributed-systems complexity to solve a problem zirv
doesn't have.

**100+ agent catalog.** This is markdown persona inflation, not capability. A `SKILL.md` file
that renames a general-purpose model call "backend-architect-agent" doesn't give it any
capability the underlying model didn't already have, and Ruflo's own internal docs undercut
the "100+" framing (98, elsewhere). zirv's roster model — real harnesses (claude, codex),
real models, real usage windows — is a smaller, honest surface; multiplying it with persona
markdown would be pure marketing surface area with no functional payoff.

**SONA neural layer.** Opaque ML has no place in zirv's determinism posture. `rot.rs` is
pure by design specifically so that identical events produce identical verdicts, auditable
and testable without a model in the loop. A neural "learning" layer over agent behavior is
exactly the kind of non-reproducible, unauditable component that contract exists to keep out
— and Ruflo's own code (`LearningBridge` "degrades gracefully when unavailable") suggests
even Ruflo doesn't fully trust it as load-bearing.

**Plugin marketplace / npm ecosystem.** zirv is one deterministic Rust binary with a narrow,
audited config/script surface (`.zirv/` scripts, `ctx.toml`, `.settings.toml`). A plugin
marketplace pulling third-party npm packages (or their markdown-bundle equivalent) into the
runtime is the opposite of that: it turns the trust boundary from "one binary I built and can
audit" into "whatever a plugin author shipped this week." zirv's repo-owned-surfaces-are-
untrusted posture (`<repo>/.zirv/{ctx.toml,system-prompt.md,context/*.md,memory/}` may only
narrow, never widen) is fundamentally incompatible with an ecosystem model built around
widening what a checkout can pull in.

**Cross-org federation.** Federation exists to let independent organizations' swarm instances
coordinate across a trust boundary. zirv has no multi-org deployment story and no product
need for one — it's a per-developer/per-repo CLI, not a hosted service coordinating between
tenants. Building federation support ahead of any concrete requirement would be solving a
problem nobody using zirv has.

**Build-provenance witness (`ruflo verify`).** zirv's own CD pipeline already ships
checksummed releases (musl static Linux binary since v2.39.1, versioned Cargo release
artifacts, GitHub Releases as the distribution channel). Adding a Ruflo-style SHA-256+Ed25519
provenance witness on top would duplicate that with low marginal value — zirv's release
posture is not the class of large, loosely-governed dependency supply chain this component
was built to defend, and there is no reported gap in the current release process it would
close.

## 4. Adopted spike candidates

Five ideas are worth a scoped spike, all re-derived in zirv's own idiom (deterministic Rust,
no opaque ML, no new runtime dependency ecosystem) rather than ported or depended on.

### a. Deterministic memory ranking/recall over `.zirv/memory/` + ctx memory

**What Ruflo does:** HNSW vector search + PageRank/label-propagation graph ranking
(`@claude-flow/memory`) to recall relevant prior memory entries — the credible half of its
"ReasoningBank" story.

**zirv-shaped version:** the ReasoningBank *idea* without vectors or an embedding model — a
BM25-style keyword+recency ranking function over `.zirv/memory/` entries and ctx memory,
pure and deterministic (same inputs, same ranked order every time), so it can sit next to
`rot.rs` under the same no-fs/clock/env/net-in-the-scoring-core discipline (I/O stays in the
caller, scoring stays pure).

**Scope:** M — new pure ranking module, a scored-recall API for whatever currently reads
`.zirv/memory/` flat, plus tests pinning ranking order on fixed inputs; no schema change to
stored memory required.

**Follow-up issue: TBD**

### b. Auto-triggered lifecycle workers on workflow gate transitions

**What Ruflo does:** 12 background workers that auto-trigger off swarm lifecycle events.

**zirv-shaped version:** zirv's workflow engine already has real gate transitions
(classify → plan → implement → test/verify → review → ship) — wire those transitions to
spawn scoped test/review workers automatically (e.g. a review-run worker fires the moment a
gate moves into the review step) instead of requiring an operator or orchestrator to remember
to dispatch one by hand. This reuses existing `zirv agent`/dashboard-pane spawn machinery; the
new part is the transition→spawn wiring itself, gated the same way `--workdir` and pane spawn
requests already are.

**Scope:** M — hook into `src/commands/workflow/{engine,classify}.rs` gate-transition points,
new spawn-on-transition config (operator-controlled, matching `REPO_FORBIDDEN` posture for
anything that changes what gets auto-launched), tests per transition.

**Follow-up issue: TBD**

### c. Deterministic prompt-injection/PII screening on transcript/mail/inbox surfaces

**What Ruflo does (via the sibling `midstream` repo, not Ruflo's own TS code):** AIDefence —
tested, `unsafe_code = deny` Rust crates (`aimds-core/detection/analysis/response`) doing
AI-specific threat detection. This is the one genuinely credible Ruflo-adjacent component
found in this evaluation.

**zirv-shaped version:** study the `aimds-*` crates' design (detection categories, rule
structure, how they avoid opaque ML) and re-derive a narrower, zirv-owned screening pass over
the surfaces that actually carry untrusted or cross-session text today — transcript content
feeding rot scoring, `zirv ctx mail`/inbox payloads, and repo-owned context files already
flagged untrusted in `docs/obsidian/Concepts/Untrusted Configuration.md`. Re-derive, don't
depend: no new external crate dependency, since a security-relevant component pulled in from
outside the workspace is exactly the kind of unaudited-supply-chain risk zirv's posture exists
to avoid.

**Scope:** L — new pure detection module (rule-based, deterministic, testable the way `rot.rs`
is), integration points across mail/transcript/inbox read paths, and a real ruleset informed
by, not copied from, AIDefence's category design.

**Follow-up issue: TBD**

### d. Hook-surface gap analysis vs Ruflo's documented 18 hooks

**What Ruflo does:** documents (per its `hooks-automation` skill, not the inflated "27"
marketing figure) 18 lifecycle hooks dispatched via CLI into `settings.json`.

**zirv-shaped version:** a straight comparison of Ruflo's 18 documented hook points against
zirv's own hook surface (`Stop`/`UserPromptSubmit`/etc. in `src/commands/ctx/hook.rs` and
related) to find genuine coverage gaps — not to match Ruflo's count, but to check whether any
of its 18 name a lifecycle moment zirv currently has no hook for at all.

**Scope:** S — a research/comparison pass producing a short gap list; no code change unless a
real gap is found, in which case it becomes its own follow-up.

**Follow-up issue: TBD**

### e. Usage-window-aware harness/model routing for `zirv agent`

**What Ruflo does:** multi-provider routing across its swarm.

**zirv-shaped version:** zirv already has the ingredients — the harness/model roster and live
usage-window tracking (`Usage and Pacing`) — that Ruflo's routing claims to solve with a
heavier mechanism. Extend `zirv agent`'s dispatch to route by live usage-window headroom
across the roster automatically (this is close in spirit to the existing cross-harness
fallback decision for exhausted vendor seats — issue #186 — but for proactive routing rather
than reactive fallback after a hit limit).

**Scope:** M — extends `src/commands/ctx/{fallback,agent,pace}.rs`'s existing usage-window
reads into a routing decision at dispatch time rather than only a post-hoc fallback; tests
per routing decision.

**Follow-up issue: TBD**

## 5. Conclusion

zirv's deterministic posture is a real differentiator, not a stylistic preference measured
against Ruflo. Ruflo's swarm is, by its own internal documentation, largely prompt-dispatch
markdown wrapped around a modest — but genuinely tested — TypeScript coordination core, with
headline numbers (100+ agents, 210+ tools, 35 plugins, 27 hooks) that don't survive a check
against Ruflo's own docs. The one component worth taking seriously on its engineering merits
alone — AIDefence's Rust crates — isn't even owned by Ruflo itself. Nothing here changes
zirv's architecture: no consensus protocol, no neural layer, no plugin ecosystem, no
federation. The five spikes above are narrow, deterministic, zirv-owned re-derivations of the
ideas that had real substance underneath the marketing, scoped for follow-up issues once
prioritized.
