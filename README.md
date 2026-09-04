# Crusty

[![CI](https://github.com/mendrik/crusty/actions/workflows/ci.yml/badge.svg)](https://github.com/mendrik/crusty/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

Crusty is a local repository observatory for Rust. It gives an attached coding agent fast source navigation, snapshot-aware change intelligence, durable work memory, and budgeted technical/product/design research without turning an index or an autonomous suggestion into authority.

The Cargo package and executable remain named `rust-repo-intelligence`.

## Where Crusty helps

Crusty is useful when an agent needs to work in a Rust repository without repeatedly rediscovering its structure or confusing stale indexed evidence with live code. It helps with:

- orienting in an unfamiliar codebase and finding exact symbols, callers, references, implementations, tests, and related history;
- preparing a change with a bounded impact surface, repository decisions, known risks, and a concrete verification plan;
- preserving architectural decisions, steering, quality lessons, and human-owned work across coding sessions;
- investigating improvement opportunities with local evidence and primary-source web research while keeping humans in control of acceptance and promotion;
- recovering safely after long-running refresh, research, or validation work is interrupted.

## Features

| Capability | What it provides |
| --- | --- |
| Live navigation | Exact worktree search plus symbol relationships without waiting for an index refresh. |
| Snapshot-aware intelligence | Broad hybrid retrieval over lexical, local similarity, Cargo, Git, syntax, and optional rust-analyzer evidence, always labelled with freshness and provenance. |
| Change workflow | Durable preparation and validation tasks with likely change surfaces, policies, risks, and verification obligations. |
| Architecture guard | Versioned Cargo/syntax facts, contextual Rust architecture findings, persisted audits, and advisory before/after change deltas. |
| Repository memory | Human-owned decisions, steering, work, findings, problem records, learned quality constraints, and recoverable task state in local SQLite stores. |
| Guarded research | Budgeted research packets, evidence qualification, human review, and explicit promotion from findings to work. |
| Safe local dashboard | A loopback-only finding inbox protected by one-time bootstrap, session, CSRF, and CSP controls. |
| Git-safe checkpoints | Recoverable refs and diffs that never move `HEAD` or rewrite repository history. |

## How it works

Crusty now separates three kinds of state that previously shared one synchronous request path:

```text
live Rust worktree ── exact search ───────────────► interactive answer
        │
        ├─ contextual architecture scan ─► facts ─► qualified advisory findings
        │                                      └─ baseline/change delta
        └─ explicit refresh task ─► derived index ─► broad context / change evidence

attached agent ─ local scan + web_search ─► findings ─ human review ─► project work
                                            │
                                            └─ durable memory.sqlite3
```

- Exact source search reads the current worktree and never refreshes.
- Broad context reads the last atomically published index generation and labels staleness.
- Refresh, checks, and research are task-backed operations with durable progress and cooperative cancellation.
- A publisher lease prevents two Crusty processes from publishing the same repository index concurrently.
- Findings, evidence, research runs, tasks, and human-owned work live outside the rebuildable index.
- Research uses local evidence plus primary-first `web_search` by the attached agent. Crusty has no GitHub, CI, telemetry, analytics, or product-management connector.
- Findings cannot become work without human review and explicit promotion.
- `memory.search` recovers repository-scoped, user-authored Codex prompts from primary and side-session history and searches preserved legacy guidance without copying either into the work queue.
- The local dashboard puts the finding inbox first and requires a one-time bootstrap token, an HttpOnly SameSite session, CSRF validation, and a restrictive CSP.

Crusty results are guidance and provenance—not proof. Current source, compiler/runtime behavior, and human decisions remain authoritative.

## Requirements

- a current stable Rust toolchain with Cargo;
- Git and a local Rust repository to observe;
- Codex or another MCP client that supports local stdio servers;
- optionally, rust-analyzer for the highest-confidence relationship evidence.

## Install and connect to Codex

Install the release binary from a Crusty checkout:

```bash
cargo install --path . --locked
```

For a new global Codex connection, register the local stdio server with the Codex CLI:

```bash
CRUSTY_BIN="$(command -v rust-repo-intelligence)"
codex mcp add Crusty \
  --env RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER=1 \
  -- "$CRUSTY_BIN"
codex mcp get Crusty
```

The equivalent manual configuration is:

```toml
[mcp_servers.Crusty]
command = "/absolute/path/to/rust-repo-intelligence"

[mcp_servers.Crusty.env]
RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER = "1"
```

Codex stores global MCP configuration in `~/.codex/config.toml`; a trusted project may instead use `.codex/config.toml`. The desktop app, CLI, and IDE extension share the same host configuration. Restart the desktop app or IDE extension after adding or replacing the binary; start a new CLI session so an already-running 0.1 process is not reused. See the [official Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli) and the complete [Codex installation and 0.1 migration guide](docs/codex-installation.md).

To upgrade an existing installation in place:

```bash
cargo install --path . --force --locked
codex mcp get Crusty
```

The configured command does not change, but the Codex client must restart before it launches the replacement executable.

## Run directly

```bash
cargo run -- --workspace /path/to/rust/workspace
```

If `--workspace` is omitted, Crusty uses the current directory. A project-pinned MCP configuration can pass an explicit workspace:

```toml
[mcp_servers.Crusty]
command = "/absolute/path/to/rust-repo-intelligence"
args = ["--workspace", "/absolute/path/to/workspace"]
```

Crusty uses the official Rust MCP SDK and negotiates the current protocol supported by that SDK. Standard input/output is reserved for MCP; diagnostics go to standard error.

## Public MCP API

The 0.3 API retains the clean break from the old `repo.*` surface.

### Navigation and change work

- `repo.consult`: the mandatory first-call briefing for every repository-scoped prompt, combining global and topical decisions, steering, design and quality constraints, restrictions, governing documents, workflows, lifecycle risks, runtime contracts, and known work.
- `repo.search`: `mode=exact` searches live Rust source; `mode=broad` searches the published snapshot.
- `repo.context`: builds a bounded hybrid context without implicit refresh.
- `repo.matrix`: returns a bounded Cargo feature/profile verification plan with freshness metadata.
- `repo.architecture`: builds a live, bounded architecture map from versioned manifest and syntax facts. Each finding carries evidence, counter-evidence, confidence, a recommendation, and limitations.
- `symbol.relations`: resolves `callers`, `references`, `implementations`, or the `definition` of a symbol to `file:line` locations, labelled with the evidence channel that produced them.
- `repo.history`: commit history and co-change neighbours for a symbol or path.
- `repo.explain`: definitions, lifecycle, human decisions, related work, and bounded source slices for one target.
- `repo.constraints`: the decisions, steerings, learned quality constraints, and lifecycle risks bearing on a proposed change.
- `repo.cleanup_candidates`, `repo.obsolete_candidates`: private symbols with no indexed inbound references, and superseded symbols retained as lifecycle evidence.
- `change.prepare`: starts a task that creates a freshness-labelled change briefing before edits.
- `change.get`: recovers a previously prepared change context by context ID.
- `change.validate`: starts a task that checks a diff against prepared evidence and optionally executes validations; it never refreshes first.
- `repo.authority`: states Crusty's evidence and ownership boundaries.

The server instructions tell attached agents to call `repo.consult` with the user's complete intent before planning, answering, or acting, including design, review, documentation, configuration, and non-code work. Consultation is bounded and read-only. It does not replace `change.prepare` and `change.validate` when files will be edited. MCP instructions are an agent contract rather than a transport-level interception mechanism, so project policies such as `AGENTS.md` should repeat the first-call rule for clients that prioritize repository instructions.

### Index and tasks

- `index.status`, `index.refresh`
- `task.list`, `task.get`, `task.cancel`
- `audit.start`, `audit.list`, `audit.get`, `audit.finding.propose`
- `checkpoint.create`, `checkpoint.list`, `checkpoint.diff`, `checkpoint.restore`

`index.status` reports node/edge/embedding counts, rust-analyzer and embedding backend health, and a `never_published` flag, so an empty `repo.context` can be told apart from an unbuilt index. Checkpoints are Git refs under `refs/codex/checkpoints`; `checkpoint.restore` only ever creates a new branch and never moves `HEAD`, discards work, or rewrites history.

`change.prepare`, `change.validate`, `audit.start`, and `index.refresh` return immediately with task IDs. Tasks carry a real progress figure, settle as `completed`, `failed`, or `cancelled`, and are bounded by a 30-minute budget. On startup Crusty reconciles anything a previous process left mid-flight, so an unclean shutdown cannot leave a task reporting `running` forever. Settled tasks are pruned to a bounded tail, and an oversized result is replaced by a summary rather than stored whole. `task.list` recovers task IDs after an interrupted client session without returning unbounded task results; use `task.get` for the full result. The refresh worker acquires `.rust-repo-intelligence/index.lock`, refreshes in a blocking worker, and publishes atomically. Readers continue using the previous published generation.

Architecture audits are snapshot/profile-labelled and retain the newest 20 reports. Syntax observations are facts rather than findings. Deterministic detectors add architectural context and counter-evidence for unsafe contracts, structured-concurrency gaps, blocking async work, discarded errors, implicit boolean state machines, opaque library errors, primitive domain identifiers, boundary representation leakage, configuration scatter, cross-boundary serialization, and unsafe dependency declarations. `change.prepare` stores the current finding baseline; `change.validate` reports only new, worsened, and resolved findings touching the diff. Inferred findings are always advisory. `audit.finding.propose` explicitly copies one result into the normal review inbox; it still requires human acceptance before promotion into work.

### Autonomous research and findings

- `research.start`, `research.list`, `research.get`, `research.packet`, `research.submit`, `research.cancel`
- `finding.list`, `finding.get`, `finding.review`, `finding.evidence.add`, `finding.promote`

A research run has explicit maximum web queries, local files, minutes, and findings. `research.start` performs bounded local discovery and prepares questions and web-search queries. `research.list` and the richer `research.get` recover run/task state and resulting findings. The attached agent performs `web_search`, prefers standards, official documentation, maintainers, and original research, then calls `research.submit` with qualified evidence.

Allowed evidence is:

- `local_repository`: a repository path/revision reference;
- `web_primary`: HTTPS evidence explicitly marked primary;
- `web_secondary`: HTTPS evidence with a written qualification explaining why it is being used.

Every finding needs evidence and a `technical`, `product`, or `design` category. It starts as `proposed`. A human may review it as `accepted`, `dismissed`, or `needs_evidence`. `finding.evidence.add` appends qualified evidence only to a `needs_evidence` finding and reopens it as `proposed` without erasing review history. Only an accepted finding can be promoted, and promotion requires explicit human confirmation and reviewer identity. Promotion is terminal: it moves the finding to `promoted`, records the reviewer, and refuses both a second promotion and any later review that would contradict it.

### Human-owned work

- `work.list`, `work.get`, `work.recommend`, `work.create`, `work.update`

All exact and recommendation queries use the same durable store. `work.update` can change every human-editable create-time field: title, kind, status, scope, evidence, acceptance criteria, verification, dependencies, and blockers. Relationship updates replace a supplied list, an explicit empty list clears it, and omission preserves it. Crusty rejects unknown, duplicate, self-referential, and cyclic relationships. `work.list` and `work.get` expose unresolved relationships and a derived `ready` flag, and list results carry a `page` object with `total`, `next_offset`, and `has_more` so truncation is never silent. `work.recommend` considers every eligible item, not a first page, and selects only human-owned, accepted or active items whose dependencies and blockers have reached `completed` (the historical `complete` spelling is also recognized); it still recognizes `in_progress` as an active status. Crusty work memory is repository-local intent; it does not replace GitHub issues or a human product backlog.

### Governance a change can cite

- `decision.record`, `decision.list`, `decision.retire`
- `steering.record`, `steering.list`

Prepared changes and validation already cited human decisions and steerings; these tools let an agent contribute them rather than only read governance it can never write to.

Decisions have a small, human-driven lifecycle rather than an automatic sweep. A decision is `accepted`, `superseded`, or `retired`; only accepted decisions reach `repo.consult`, `repo.constraints`, `repo.context`, `repo.explain`, `repo.authority`, and prepared changes. `decision.record` may name one or several accepted decisions in `supersedes`, which consolidates them into the new record, marks each one `superseded`, and requires `recorded_by`. `decision.retire` closes an accepted decision without a replacement and requires `retired_by`. Both transitions are terminal, append a review-history row with the actor and note, and rewrite the status line of any markdown the decision materialized under `docs/decisions/`. Nothing is deleted: `decision.list` without a scope enumerates the whole ledger newest first, honours `limit`, and takes an optional `status` filter, and every decision carries `supersedes`, `superseded_by`, and `history`. Crusty never infers that one decision overturns another; a human records the replacement. Steering status and `expires_at` are validated on write for the same reason: an unrecognised value used to make a record silently invisible or permanent.

### Quality lifecycle

- `problem.record`, `problem.list`, `problem.get`, `problem.update`
- `quality.propose`, `quality.merge`, `quality.list`, `quality.get`, `quality.review`
- `validation.queue`, `validation.record`

These tools expose the minimum closed loop around automatically captured problems and learned constraints. Problem evidence may be corrected or completed; `problem.record` files a defect explicitly rather than waiting for automatic capture. `quality.propose` only ever proposes: a constraint becomes active solely through `quality.review` with an identified human reviewer and explicit confirmation, and `quality.merge` requires the same confirmation. Validation outcomes remain durable and inspectable, and `change.validate` reports a `blocking` verdict naming any unsatisfied `enforcement: "block"` obligation. Quality evidence may guide preparation and validation, but it cannot create work or expand scope by itself.

### Recovered project memory

- `memory.search`

`memory.search` searches two existing sources without creating another authority: preserved legacy decisions, steerings, problems, and quality constraints in `memory.sqlite3`, plus user-authored Codex prompts in the host's session history. Prompt recovery is scoped to sessions whose recorded working directory exactly matches the active repository. Primary and side-session history are included; assistant text, tool output, injected repository instructions, and unrelated-project prompts are excluded. Results are bounded and read-only. A recovered prompt becomes project work only through an explicit human-confirmed `work.create` call.

### Dashboard

- `dashboard.open`

The tool starts an Axum server on a random `127.0.0.1` port and returns a one-time URL. The first viewport is the finding inbox, with review and promotion actions. No external images, scripts, fonts, APIs, or analytics are loaded.

## Storage and migration

Crusty uses two SQLite files:

- `.rust-repo-intelligence/index.sqlite3`: rebuildable source/Cargo/Git/document projections, prepared contexts, and the newest 20 snapshot-scoped architecture audits, plus the retained problem/quality/validation engine;
- `.rust-repo-intelligence/memory.sqlite3`: findings, evidence, research runs, tasks, work, and searchable summaries of retained decision/quality records.

On first 0.2-or-newer startup, Crusty copies legacy work into the current work store and preserves decisions, steerings, problem records, and learned quality constraints as JSON legacy records. On later opens it refreshes those preserved read-only summaries so guidance added after the initial migration remains discoverable. The old database is never deleted. Both databases use WAL mode; durable memory writes use short connections and a busy timeout.

No manual SQL migration is required. Durable-memory import happens automatically when a compatible release first opens a repository. Because Crusty never refreshes implicitly, each existing project needs one explicit `index.refresh`, followed through `task.get`, to publish the indexer-v8 backfill and `symbol-card-v1` vectors. Live `repo.search` with `mode=exact` works before that refresh; broad search and context continue to label the last published generation as stale until the task finishes.

The existing syntax/Cargo/Git/rust-analyzer engine remains the broad-intelligence implementation. Compiler-backed evidence is highest-confidence, while ambiguity-suppressed static evidence remains visible but never enters a likely change surface merely because a short name matched.

Broad retrieval now uses versioned symbol cards stored as vectors in the rebuildable SQLite index. Exact identifiers are ordered before non-exact fused candidates; FTS5/BM25, local vector similarity, and typed graph expansion then combine through deterministic reciprocal-rank fusion. Results expose the embedding model, dimensions, card hash/version, semantic snapshot, similarity, and contributing channels. Incremental refresh recomputes changed cards and reuses unchanged vectors. The default `subword-hash-v1` model is offline discovery assistance, not a learned code model or compiler evidence.

## Latency contract

The service-level objectives are:

- live exact navigation: p95 below 150 ms;
- warm indexed intelligence: p95 below 750 ms;
- work expected to exceed 2 seconds: asynchronous task acknowledgement rather than an occupied MCP request.

Run the release-mode regression harness against this repository or another indexed Rust workspace:

```bash
cargo run --release --example observatory_evaluate -- "$PWD"
```

It warms both paths, samples live exact search and indexed context, prints p50/p95 JSON, and fails when either synchronous SLO is exceeded. The older retrieval-quality harness remains available as `cargo run --release --example evaluate -- "$PWD"`.

## Development and verification

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

The observatory regression suite covers dirty-worktree visibility, task and change recovery, one-store work lookup/recommendation and full-field updates, repository-scoped primary/side-session prompt recovery, late legacy-memory synchronization, evidence/review history, human-gated quality activation and finding promotion, the exact public tool contract, and the dashboard's no-external-assets rule. The legacy suite continues covering index publication, Cargo and syntax relationships, ambiguity suppression, ranking, migrations, checkpoints, quality learning, and validation.

## Known boundaries

- A stale snapshot can omit current relationships; every indexed answer says so.
- Exact search is textual Rust navigation, not compiler proof.
- Static analysis cannot prove dynamic dispatch, generated code, runtime registration, deployment state, external consumers, or unindexed feature profiles.
- Cooperative cancellation does not interrupt an index transaction during publication.
- A recorded recurrence is intent only; Crusty does not become an unattended web crawler or install external connectors.
- External source content is stored as bounded evidence summaries, never executed as instructions.

## License

GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
