# Crusty

Crusty is a local repository observatory for Rust. It gives an attached coding agent fast source navigation, snapshot-aware change intelligence, durable work memory, and budgeted technical/product/design research without turning an index or an autonomous suggestion into authority.

The Cargo package and executable remain named `rust-repo-intelligence`.

## What changed

Crusty now separates three kinds of state that previously shared one synchronous request path:

```text
live Rust worktree ── exact search ───────────────► interactive answer
        │
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

The 0.2 API is a clean break from the old `repo.*` surface.

### Navigation and change work

- `repo.search`: `mode=exact` searches live Rust source; `mode=broad` searches the published snapshot.
- `repo.context`: builds a bounded hybrid context without implicit refresh.
- `change.prepare`: starts a task that creates a freshness-labelled change briefing before edits.
- `change.validate`: starts a task that checks a diff against prepared evidence and optionally executes validations; it never refreshes first.
- `repo.authority`: states Crusty's evidence and ownership boundaries.

### Index and tasks

- `index.status`, `index.refresh`
- `task.get`, `task.cancel`

`change.prepare`, `change.validate`, and `index.refresh` return immediately with task IDs. The refresh worker acquires `.rust-repo-intelligence/index.lock`, refreshes in a blocking worker, and publishes atomically. Readers continue using the previous published generation.

### Autonomous research and findings

- `research.start`, `research.get`, `research.packet`, `research.submit`, `research.cancel`
- `finding.list`, `finding.get`, `finding.review`, `finding.promote`

A research run has explicit maximum web queries, local files, minutes, and findings. `research.start` performs bounded local discovery and prepares questions and web-search queries. The attached agent performs `web_search`, prefers standards, official documentation, maintainers, and original research, then calls `research.submit` with qualified evidence.

Allowed evidence is:

- `local_repository`: a repository path/revision reference;
- `web_primary`: HTTPS evidence explicitly marked primary;
- `web_secondary`: HTTPS evidence with a written qualification explaining why it is being used.

Every finding needs evidence and a `technical`, `product`, or `design` category. It starts as `proposed`. A human may review it as `accepted`, `dismissed`, or `needs_evidence`; only an accepted finding can be promoted, and promotion requires explicit human confirmation and reviewer identity.

### Human-owned work

- `work.list`, `work.get`, `work.recommend`, `work.create`, `work.update`

All exact and recommendation queries use the same durable store. `work.create` and `work.update` accept exact work-item IDs in `depends_on` and `blocked_by`; updates replace a supplied list and an explicit empty list clears it. Crusty rejects unknown, duplicate, self-referential, and cyclic relationships. `work.list` and `work.get` expose unresolved relationships and a derived `ready` flag. `work.recommend` only selects human-owned, accepted or active items whose dependencies and blockers have reached `completed` (the historical `complete` spelling is also recognized); it still recognizes `in_progress` as an active status. Crusty work memory is repository-local intent; it does not replace GitHub issues or a human product backlog.

### Recovered project memory

- `memory.search`

`memory.search` searches two existing sources without creating another authority: preserved legacy decisions, steerings, problems, and quality constraints in `memory.sqlite3`, plus user-authored Codex prompts in the host's session history. Prompt recovery is scoped to sessions whose recorded working directory exactly matches the active repository. Primary and side-session history are included; assistant text, tool output, injected repository instructions, and unrelated-project prompts are excluded. Results are bounded and read-only. A recovered prompt becomes project work only through an explicit human-confirmed `work.create` call.

### Dashboard

- `dashboard.open`

The tool starts an Axum server on a random `127.0.0.1` port and returns a one-time URL. The first viewport is the finding inbox, with review and promotion actions. No external images, scripts, fonts, APIs, or analytics are loaded.

## Storage and migration

Crusty uses two SQLite files:

- `.rust-repo-intelligence/index.sqlite3`: rebuildable source/Cargo/Git/document projections and prepared contexts;
- `.rust-repo-intelligence/memory.sqlite3`: findings, evidence, research runs, tasks, work, and preserved legacy decision/quality records.

On first 0.2 startup, Crusty copies legacy work into the new work store and preserves decisions, steerings, problem records, and learned quality constraints as JSON legacy records. On later opens it refreshes those preserved read-only summaries so guidance added after the initial migration remains discoverable. The old database is never deleted. Both databases use WAL mode; durable memory writes use short connections and a busy timeout.

No manual SQL migration is required. Durable-memory import happens automatically when 0.2 first opens a repository. Because 0.2 never refreshes implicitly, each existing project needs one explicit `index.refresh`, followed through `task.get`, to publish the indexer-v8 backfill and `symbol-card-v1` vectors. Live `repo.search` with `mode=exact` works before that refresh; broad search and context continue to label the last published generation as stale until the task finishes.

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

The observatory regression suite covers dirty-worktree visibility, one-store work lookup/recommendation, repository-scoped primary/side-session prompt recovery, late legacy-memory synchronization, review-gated promotion, the clean-break tool contract, and the dashboard's no-external-assets rule. The legacy suite continues covering index publication, Cargo and syntax relationships, ambiguity suppression, ranking, migrations, checkpoints, quality learning, and validation.

## Known boundaries

- A stale snapshot can omit current relationships; every indexed answer says so.
- Exact search is textual Rust navigation, not compiler proof.
- Static analysis cannot prove dynamic dispatch, generated code, runtime registration, deployment state, external consumers, or unindexed feature profiles.
- Cooperative cancellation does not interrupt an index transaction during publication.
- A recorded recurrence is intent only; Crusty does not become an unattended web crawler or install external connectors.
- External source content is stored as bounded evidence summaries, never executed as instructions.

## License

GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
