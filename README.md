# Crusty

[![CI](https://github.com/mendrik/crusty/actions/workflows/ci.yml/badge.svg)](https://github.com/mendrik/crusty/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

Crusty is a local repository observatory and engineering service for Rust. It gives an attached coding agent live Rust semantics, project-specific guidance, revision-bound checks, durable work memory, coordinated coding sessions, and GitHub delivery under explicit policy.

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
| Rust engineering | Conditional ownership/API/architecture/cleanup/concurrency/unsafe/performance guidance and human-reviewed domain models. |
| Live LSP queries | File-position hover, definitions, references, implementations, call hierarchy, macros, dependency information and proposed edits, independent of index publication. |
| Verification | Current Cargo/instruction contracts, explicit feature/target/toolchain profiles, bounded compiler diagnostics and source/environment-bound results. |
| Coordinated delivery | Shared sessions and claims, coherent exact-tree commits, isolated conflict resolution, draft PRs, revision-bound reviews, approvals and reconciled merge requests. |
| Measured optimization | Pinned baseline/candidate release workloads, instrumented timings, output oracles and retained sample artifacts. |

## How it works

Crusty now separates three kinds of state that previously shared one synchronous request path:

```text
live Rust worktree ── exact search ───────────────► interactive answer
        │
        ├─ contextual architecture scan ─► facts ─► qualified advisory findings
        │                                      └─ baseline/change delta
        └─ refresh task (background or explicit) ─► derived index ─► broad context / change evidence

attached agent ─ local scan + web_search ─► findings ─ human review ─► project work
                                            │
                                            └─ durable memory.sqlite3
```

- Exact source search reads the current worktree and never refreshes.
- Broad context reads the last atomically published index generation and labels staleness with a reason; it never waits for a refresh.
- A background refresher (one per server process) republishes the index after edits and commits; set `CRUSTY_AUTO_REFRESH=0` to keep refresh explicit.
- Refresh, checks, and research are task-backed operations with durable progress and cooperative cancellation.
- A publisher lease prevents two Crusty processes from publishing the same repository index concurrently.
- Findings, evidence, research runs, tasks, and human-owned work live outside the rebuildable index.
- Research uses local evidence plus primary-first `web_search` by the attached agent. GitHub delivery uses the installed `gh` CLI with an explicit repository and bounded human policy; remote CI and protections remain authoritative.
- Findings cannot become work without human review and explicit promotion.
- `memory.search` recovers repository-scoped, user-authored prompts from Codex primary and side-session history and from Claude Code transcripts, and searches preserved legacy guidance without copying either into the work queue.
- The local dashboard puts the finding inbox first and requires a one-time bootstrap token, an HttpOnly SameSite session, CSRF validation, and a restrictive CSP.

Crusty results are guidance and provenance—not proof. Current source, compiler/runtime behavior, and human decisions remain authoritative.

## Requirements

- a current stable Rust toolchain with Cargo;
- Git and a local Rust repository to observe;
- Codex or another MCP client that supports local stdio servers;
- optionally, rust-analyzer for the highest-confidence relationship evidence.
- optionally, an authenticated GitHub CLI (`gh`) for PR delivery and reviews.

## Install and connect to Codex

Install the release binary from a Crusty checkout:

```bash
cargo install --path . --locked
```

For a new global Codex connection, register the local stdio server with the Codex CLI:

```bash
CRUSTY_BIN="$(command -v rust-repo-intelligence)"
codex mcp add Crusty -- "$CRUSTY_BIN"
codex mcp get Crusty
```

The equivalent manual configuration is:

```toml
[mcp_servers.Crusty]
command = "/absolute/path/to/rust-repo-intelligence"```

rust-analyzer is off by default; agents call `semantic.enable` when they need live semantics or diagnostics and `semantic.disable` to release it. To start it with every server instead, add `--env RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART=1` (or the same key under `[mcp_servers.Crusty.env]`). The older `RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER` no longer starts it and can be removed.

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
- `repo.context`: builds a bounded hybrid context from the last published generation without waiting for a refresh.
- `repo.matrix`: returns a bounded Cargo feature/profile verification plan with freshness metadata.
- `repo.architecture`: builds a live, bounded architecture map from versioned manifest and syntax facts. Each finding carries evidence, counter-evidence, confidence, a recommendation, and limitations.
- `symbol.relations`: resolves `callers`, `references`, `implementations`, or the `definition` of a symbol to `file:line` locations, labelled with the evidence channel that produced them.
- `repo.history`: commit history and co-change neighbours for a symbol or path.
- `repo.explain`: definitions, lifecycle, human decisions, related work, and bounded source slices for one target.
- `repo.constraints`: the decisions, steerings, learned quality constraints, and lifecycle risks bearing on a proposed change.
- `repo.cleanup_candidates`, `repo.obsolete_candidates`: private symbols with no indexed inbound references, and superseded symbols retained as lifecycle evidence.
- `change.prepare`: starts a task that creates a freshness-labelled change briefing before edits, including the consultation guidance for the change. One prepared context serves every validation of a coherent change.
- `change.get`: recovers a previously prepared change context by context ID.
- `change.validate`: starts a task that checks the pending worktree change (default), an inline diff, local patch file, or server-side Git comparison against prepared evidence and optionally executes validations; it never refreshes first. `context_id` is optional.
- `repo.authority`: states Crusty's evidence and ownership boundaries.

The server instructions tell attached agents to consult before planning, answering, or acting on any repository-scoped prompt, including design, review, documentation, configuration, and non-code work: `repo.consult` with the user's complete intent, or, for an edit task, `change.prepare`, whose result carries the same decisions, steering, live instructions, and engineering route. Consultation is bounded and read-only. The edit workflow is deliberately proportionate: one `change.prepare` per coherent change, and `change.validate` at milestones and at the end with the same context ID, however long an iterative refactor runs. Sessions and path claims are only for parallel work. MCP instructions are an agent contract rather than a transport-level interception mechanism, so project policies such as `AGENTS.md` should repeat the first-call rule for clients that prioritize repository instructions.

Every task-starting tool (`change.prepare`, `change.validate`, `session.start`, `index.refresh`, `audit.start`, `commit.plan/execute`, `verification.plan/run`, `integration.preview/start/resolve/complete`, `cleanup.plan`, `performance.measure`, `semantic.query`, and the GitHub tasks) and `task.get` accept an optional `wait_seconds` (default 0, at most 120). When the task settles within the wait, the call returns the settled task in `task.get` shape; otherwise it returns the task ID with its current status, as without a wait. Cancelling a waiting MCP request ends the wait; a task the request started is then cancelled too, because no one else holds its ID, while a cancelled `task.get` wait leaves the task running.

Large changes do not need to travel through tool arguments. Call `change.validate` with `{"context_id":"ctx_…","base_ref":"<base-commit>","target":"worktree"}` to include committed changes plus staged and unstaged tracked edits, or use `"target":"HEAD"` for committed changes only. `base_ref` is compared directly, with no implicit merge-base; supply the intended branch-point commit when reviewing a branch. Alternatively, pass `"diff_path":"/tmp/changes.patch"` for a local UTF-8 patch (relative paths resolve from the repository root). `git_diff`, `diff_path`, and `base_ref` are mutually exclusive; `target` requires `base_ref` and defaults to `worktree`. With no source, validation covers pending tracked edits against HEAD plus every untracked file Git does not ignore, as a new-file diff (at most 500 files of up to 1 MiB each; Crusty's state directory and untracked `target/` are skipped, and `diff_scope.untracked` lists what was included or skipped). `base_ref` comparisons keep excluding untracked files.

Without `context_id`, validation still runs the diff-driven obligations, decision conflicts, blocking verdict, legacy-path checks, artifact validators, and optional checks, and records an unprepared context (`prepared: false`) that owns those obligations. `architecture_delta`, `expected_affected_files`, and `unmodified_expected_callers` then report `{"status":"unavailable","reason":"no prepared context"}`, because they compare against evidence captured before the change. The report's `diff_scope` records the source, resolved commits where applicable, exact patch byte count, and BLAKE3 hash; `changed_files` names its scope. Analysis and optional checks always inspect the current worktree, including when the diff target is HEAD.

Validation reports include `validation_status.verdict`: `passed`, `failed`, `incomplete`, or `not_run`. A task marked `completed` means its report was produced; it does not mean the checks passed. The separate `blocking` field describes human-approved quality obligations.

`repo.consult`, `repo.context`, and prepared-change responses budget the entire logical JSON payload, including freshness and metadata, using UTF-8 bytes divided by four as an approximate token estimate. They prioritize human guidance, report omitted sections, and expose `serialized_bytes`. Consultation compacts items (id, title, path) before dropping any, reports `compacted` and `omitted` counts with a `fetch_more` hint per section, lists only open, topic-relevant known work as briefs (`work.get` returns the detail), keeps CI configuration out of unrelated topics, and stubs instruction files that are outside the topic's paths or do not fit; see [repository memory](docs/repository-memory.md#decisions-and-steering). The complete prepared evidence remains available through `change.get`. MCP's text/structured representations and framing add transport overhead; this estimate is not a measurement of model tokens or net token savings.

### Rust intelligence, verification and measured changes

`engineering.guidance` returns conditional Rust expertise with rationale, exceptions and evidence requirements. `project.contract` reads current Cargo metadata and instruction/configuration/CI evidence. `semantic.status/query/diagnostics` expose the rust-analyzer companion, which is off until `semantic.enable`: live LSP queries with explicit readiness, profile and completeness, and its captured diagnostics with per-file freshness (also added, advisory, to `change.validate`). `verification.plan/run/get` execute supported Cargo checks and preserve diagnostics and logs; default tests include doctests. `domain.model.propose/review/get/list` record ownership and invariants, activating a model only after explicit human review. `cleanup.plan/get` inventory the complete migration surface. `performance.contract/measure/get` compare measured workloads at pinned revisions. See [engineering workflow and limits](docs/engineering-workflow.md).

### Parallel sessions, commits and GitHub delivery

Sessions are for parallel work. `session.start` registers owner, intent and work references, optionally creating an isolated Git branch/worktree. `session.claim` acquires exclusive file/subtree ownership across linked worktrees; `session.heartbeat` renews its lease and activity. `session.list/get` expose who is doing what, and `session.close` releases ownership while retaining work. `commit.plan/execute/get` group and commit owned whole-file changes while preserving unrelated staging; a single agent may omit session credentials while no other session is active in the repository. `chunk.create/get/list` retain immutable deliverables. `integration.preview/start/resolve/complete/get` preview Git conflicts and validate resolutions in retained isolated worktrees. See [session coordination](docs/session-coordination.md).

`github.status`, `github.pr.list/get`, and `github.review.packet` gather explicit remote evidence. `delivery.policy.grant/get/revoke` govern publication, reviews, approvals and merges. `github.pr.publish/ready`, `github.review.submit`, and `github.pr.merge` execute authorized actions; `github.action.get/list/reconcile` recover their actual remote outcomes. See [GitHub delivery and recovery](docs/github-delivery.md), the [implementation plan](docs/implementation-plan.md), and [evaluation corpus](docs/engineering-evaluation.md).

### Index and tasks

- `index.status`, `index.refresh`
- `task.list`, `task.get`, `task.cancel`
- `audit.start`, `audit.list`, `audit.get`, `audit.finding.propose`
- `checkpoint.create`, `checkpoint.list`, `checkpoint.diff`, `checkpoint.restore`

`index.status` reports the resolved workspace root, the freshness envelope, one `index` block (snapshot, node/edge/embedding counts, rust-analyzer and embedding backend health), the latest refresh task's compact result, the background refresher, and a `never_published` flag, so an empty `repo.context` can be told apart from an unbuilt index.

Every indexed answer carries a freshness envelope. `stale` and `reason` compare the live workspace with the inputs the published generation indexed: `ok`, `never_published`, `indexer_outdated` (written by another indexer version), `head_moved`, `worktree_changed`, `refresh_running` (with the underlying `cause`), or `error` when the index or task store could not be read. Only index inputs count, so Crusty's state directory, Cargo `target/` directories, and other non-input files never make an answer stale, and a dirty worktree reads fresh once it has been refreshed. The check runs `git rev-parse` and one `git status` and hashes only inputs that are dirty now or were dirty when indexed (about 6 ms on this repository). `refresh` names a running refresh task, the last completion, and whether automatic refresh is on; `backend` is `no_published_snapshot` when nothing was ever published.

The server resolves its root before opening any state: the requested path is canonicalised and, inside a Git work tree, replaced by the nearest ancestor `Cargo.toml` declaring `[workspace]` (or else the nearest package manifest), never above the Git top level. A server started in `src/` or a member crate therefore shares the workspace store; a linked worktree keeps its own. The resolution is logged to stderr and reported as `index.status.root`.

Unless `CRUSTY_AUTO_REFRESH` is `0`, `false`, `no`, or `off`, each server runs one background refresher. It watches the root (ignoring the state directory, Cargo target directories, and Git internals other than `HEAD` and refs; `RUST_REPO_INTELLIGENCE_ENABLE_WATCHER=0` disables the watcher) and polls `HEAD` every five seconds. After two quiet seconds, or at most thirty seconds into continuous changes, it checks freshness and, only when stale, runs the incremental refresh as a durable `index.refresh` task under the publisher lease; it skips while another refresh runs in this or another process. It also runs once shortly after startup when the index is stale or never published, without delaying MCP initialisation. `policy.implicit_refresh` reports whether it is active. Reads keep serving the last published generation throughout. Checkpoints are Git refs under `refs/codex/checkpoints`; `checkpoint.restore` only ever creates a new branch and never moves `HEAD`, discards work, or rewrites history.

`change.prepare`, `change.validate`, `audit.start`, and `index.refresh` return immediately with task IDs unless `wait_seconds` asks them to return a settled task inline. Tasks carry a real progress figure and settle as `completed`, `failed`, or `cancelled`. Change, audit, and refresh workers check cancellation and a 30-minute deadline at cooperative boundaries. Validation subprocesses are terminated and reaped on cancellation or timeout; on Unix this includes their process group. Atomic index publication is allowed to finish before cancellation is acknowledged. On startup Crusty recovers abandoned session owners; a second active server preserves the first server’s tasks and research runs. Settled tasks are pruned to a bounded tail, and an oversized result is replaced by a summary rather than stored whole. `task.list` recovers task IDs after an interrupted client session without returning unbounded task results; use `task.get` for the full result. The refresh worker acquires `.rust-repo-intelligence/index.lock`, refreshes in a blocking worker, and publishes atomically. Readers continue using the previous published generation without waiting for the refresh. `index.refresh` defaults to `scope="incremental"`, which re-indexes only changed files and the symbols they affect and reuses embedding vectors by symbol-card hash; `scope="workspace"` (alias `full`) rebuilds everything, and `scope="git"` refreshes commit history only without publishing a generation.

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
- `steering.record`, `steering.list`, `steering.retire`

Prepared changes and validation already cited human decisions and steerings; these tools let an agent contribute them rather than only read governance it can never write to.

Decisions have a small, human-driven lifecycle rather than an automatic sweep. A decision is `accepted`, `superseded`, or `retired`; only accepted decisions reach `repo.consult`, `repo.constraints`, `repo.context`, `repo.explain`, `repo.authority`, and prepared changes. `decision.record` may name one or several accepted decisions in `supersedes`, which consolidates them into the new record, marks each one `superseded`, and requires `recorded_by`. `decision.retire` closes an accepted decision without a replacement and requires `retired_by`. Both transitions are terminal, append a review-history row with the actor and note, and rewrite the status line of any markdown the decision materialized under `docs/decisions/`. Nothing is deleted: `decision.list` without a scope enumerates the whole ledger newest first, honours `limit`, and takes an optional `status` filter, and every decision carries `supersedes`, `superseded_by`, and `history`. Crusty never infers that one decision overturns another; a human records the replacement. Steering status and `expires_at` are validated on write for the same reason: an unrecognised value used to make a record silently invisible or permanent.

Steerings follow the same human-driven lifecycle. `steering.record` may name active steerings in `supersedes` (with `recorded_by`), which retires them in the same transaction; `steering.retire` closes one with `retired_by` and a `reason`. Each steering exposes `supersedes`, `superseded_by`, and its `history`. `steering.list` returns only active, unexpired steerings unless `status` is `retired` or `all`, filtering before the limit. With a scope that names paths, a steering scoped to an ancestor or descendant of a named path ranks first, symbol and concept scopes keep text matching, and global steerings come last; sibling paths that share directory names no longer match. Steerings and decisions report `stale_references` to repository paths they name that no longer exist, `steering.record` warns about them, and a newer active steering with the same title and scope is reported as `possibly_superseded_by`. Consultation surfaces these hints and asks the agent to involve a human; agents never retire guidance on their own initiative.

### Quality lifecycle

- `problem.record`, `problem.list`, `problem.get`, `problem.update`
- `quality.propose`, `quality.merge`, `quality.list`, `quality.get`, `quality.review`
- `validation.queue`, `validation.record`

These tools expose the minimum closed loop around automatically captured problems and learned constraints. Problem evidence may be corrected or completed; `problem.record` files a defect explicitly rather than waiting for automatic capture. `quality.propose` only ever proposes: a constraint becomes active solely through `quality.review` with an identified human reviewer and explicit confirmation, and `quality.merge` requires the same confirmation. Validation outcomes remain durable and inspectable, and `change.validate` reports a `blocking` verdict naming any unsatisfied `enforcement: "block"` obligation. Quality evidence may guide preparation and validation, but it cannot create work or expand scope by itself.

### Recovered project memory

- `memory.search`

`memory.search` searches existing sources without creating another authority: preserved legacy decisions, steerings, problems, and quality constraints in `memory.sqlite3`, plus user-authored prompts from the host's Codex session history (`$CODEX_HOME`, default `~/.codex`) and Claude Code transcripts (`$CLAUDE_CONFIG_DIR/projects/<slug>/*.jsonl`, default `~/.claude/projects`, where the slug is the absolute repository path with every non-alphanumeric character replaced by `-`). Each prompt names its `source` (`codex` or `claude_code`) and `relevance`, and `history` reports each source separately. Prompt recovery is scoped to sessions whose recorded working directory exactly matches the active repository. Codex primary and side-session history are included; assistant text, tool output, injected repository instructions and system reminders, slash-command and shell echoes, Claude Code subagent (sidechain) and meta messages, compaction summaries, and unrelated-project prompts are excluded. Claude Code reading is bounded to the 200 most recent transcripts and 64 MiB per transcript. Results are bounded and read-only. A recovered prompt becomes project work only through an explicit human-confirmed `work.create` call.

### Dashboard

- `dashboard.open`

The tool starts an Axum server on a random `127.0.0.1` port and returns a one-time URL. The first viewport is the finding inbox, with review and promotion actions. No external images, scripts, fonts, APIs, or analytics are loaded.

## Storage and migration

Crusty uses worktree-local evidence stores and a shared coordination ledger:

- `.rust-repo-intelligence/index.sqlite3`: rebuildable source/Cargo/Git/document projections, prepared contexts, and the newest 20 snapshot-scoped architecture audits, plus the retained problem/quality/validation engine;
- `.rust-repo-intelligence/memory.sqlite3`: findings, evidence, research runs, tasks, work, and searchable summaries of retained decision/quality records.
- `<canonical Git common directory>/crusty/coordination.sqlite3`: linked-worktree sessions, claims, commit/delivery intents, accepted models, verification and measurement records. Non-Git roots use `.rust-repo-intelligence/coordination/`. Artifact logs and retained integration/measurement worktrees live beside this ledger.

On first 0.2-or-newer startup, Crusty copies legacy work into the current work store and preserves decisions, steerings, problem records, and learned quality constraints as JSON legacy records. On later opens it refreshes those preserved read-only summaries so guidance added after the initial migration remains discoverable. The old database is never deleted. Both databases use WAL mode; durable memory writes use short connections and a busy timeout.

No manual SQL migration is required. Durable-memory import happens automatically when a compatible release first opens a repository. The background refresher publishes the indexer-v9 backfill and `symbol-card-v1` vectors shortly after the server starts; with `CRUSTY_AUTO_REFRESH=0`, each existing project needs one explicit `index.refresh`, followed through `task.get`. Live `repo.search` with `mode=exact` works before that refresh; broad search and context continue to label the last published generation as stale until the task finishes.

The existing syntax/Cargo/Git/rust-analyzer engine remains the broad-intelligence implementation. The MCP server keeps one rust-analyzer companion across requests. It is off until an agent calls `semantic.enable` (or `RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART=1` starts it with the server); once enabled it is warmed in the background with cache priming (set `RUST_REPO_INTELLIGENCE_RUST_ANALYZER_WARM_START=0` to start it on first use instead), and `semantic.disable` stops it. The analyzer is resolved with `rustup which` in the workspace, so a `rust-toolchain` pin selects it, and `semantic.status` flags an analyzer more than 90 days old with an update hint; refresh never starts it. Source edits reach it as file-change and document notifications instead of restarts, and a dead process is restarted with backoff. When it is disabled, `semantic.status`, `index.status`, and `repo.consult` say how to enable it. Cached semantic results require matching live source and build-profile evidence, and source slices compare live content hashes with their indexed hashes. Compiler-backed evidence is highest-confidence, while ambiguity-suppressed static evidence remains visible but never enters a likely change surface merely because a short name matched.

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

The test suite uses Clippy and, on Unix, Python 3 for isolated stdio MCP regressions. These tests use temporary repositories and instrumented subprocesses; they do not open your project memory.

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
