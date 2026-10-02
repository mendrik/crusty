# Crusty 0.3 state, retrieval, and migration

Crusty separates live navigation, rebuildable repository intelligence, and durable human/project memory. This separation is the core authority boundary: current source and runtime/compiler behavior remain authoritative, indexed relationships are freshness-labelled guidance, and autonomous findings remain proposals until a human acts on them.

## Authority and freshness

`repo.search(mode=exact)` reads Rust source from the live worktree and never refreshes the index. It is the preferred path for identifiers, call sites, and compiler-error navigation.

`repo.search(mode=broad)` and `repo.context` read the last atomically published index generation. Their responses include a freshness envelope containing the live and indexed revisions, generation, staleness, confidence, and an explicit note that no implicit refresh was attempted.

`repo.architecture` reads current manifests and Rust syntax directly from the live worktree. Its versioned facts and contextual findings are labelled separately from the published relationship graph. `audit.start` runs the same bounded analysis as a durable background task and persists its completed report.

`index.status` reports the snapshot boundary plus the latest refresh task and publisher lock. `index.refresh` is the only public refresh entry point. It returns a task ID immediately; `task.list` recovers bounded task summaries after an interrupted session and `task.get` exposes one task's queued, running, completed, or failed state and full result. Readers keep using the previous generation while a single writer builds and publishes the replacement.

Indexed source ranges are read from the live worktree and hash-checked against indexed content. A same-length edit is still stale; returned hashes describe the current range, and missing live source has no current hash. Stale ranges have reduced confidence and explicit provenance.

The MCP server owns a persistent, opt-in rust-analyzer companion. Semantic requests start it lazily and send current document contents. Refresh does not start the companion. Cached semantic evidence is reused only when source and semantic-profile identities still match.

Cargo/toolchain/profile inputs or an indexer-version change force a full rebuild. Ordinary source changes may use incremental invalidation. The semantic snapshot includes the source/Cargo digest, `Cargo.lock`, target triple, feature profile, build-environment fingerprint, and rust-analyzer version.

## Storage

Repository-local state lives under the ignored `.rust-repo-intelligence/` directory:

- `index.sqlite3` contains rebuildable source, Cargo, Git, document, graph, search, embedding, and prepared-change projections, the newest 20 snapshot-scoped architecture audits, plus the retained problem/quality/validation engine.
- `memory.sqlite3` contains research runs, evidence, findings, task history, finding review decisions, the single human-owned work store, and searchable summaries of retained legacy guidance.
- `index.lock` is the filesystem publisher lease used to reject concurrent writers.
- `sessions/*.lock` identifies live server owners of tasks and research runs. Startup recovers only abandoned owners, while records from older versions without ownership are treated as interrupted.

Consultation, context, explanations, and preparation join current work from `memory.sqlite3` read-only, including human ownership and readiness. Long intents match work by terms; mutable work state is never copied into the derived index.

Both databases use SQLite WAL mode. Index publication is transactional, while durable-memory operations use short connections and a busy timeout. Indexing never modifies repository source files.

Codex's own session history remains owned by Codex and is not copied into either Crusty database. `memory.search` reads it on demand, uses session metadata to restrict results to the exact active repository, includes side-session records represented in the thread-history projection, excludes assistant/tool/injected-context content, and returns bounded matching user prompts with provenance. This is recovery evidence, not a new canonical roadmap.

Response budgets cover the complete logical JSON payload, including freshness and metadata, at an approximate four UTF-8 bytes per token. Governance takes priority and omitted sections are counted. `serialized_bytes` and `estimated_tokens` describe that payload, excluding MCP framing and duplicate text/structured representations. Prepared contexts are persisted in full before their bounded briefing is returned; `change.get` retrieves the complete context.

Validation reports separate task completion from check outcomes through `validation_status.verdict`. Executed failures produce `failed`; unavailable checks or unresolved blocking obligations produce `incomplete`; checks not requested produce `not_run`. Cancellation and timeout are acknowledged after validation subprocesses have been terminated and reaped. Unix process groups include Cargo's descendants; other platforms terminate the direct child. Atomic index transactions finish at a safe boundary.

## Hybrid retrieval

`repo.context` and broad search combine FTS5/BM25, deterministic local symbol-card embeddings, and intent-directed typed graph expansion. Reciprocal-rank fusion combines ranks without treating lexical and cosine scores as interchangeable. Exact symbol identities always precede non-exact fused candidates, and ranking ties and graph seeds are deterministic.

The embedding unit is `symbol-card-v1`, a bounded representation of canonical identity, module, kind, visibility, crate, file, source, and typed incoming/outgoing relationships. The card hash—not merely the source node hash—keys reuse. Incremental refresh moves unchanged vectors to the new semantic snapshot and recomputes changed cards. Status and evaluation output report the model, dimensions, card version, semantic snapshot, recomputed/reused counts, and storage strategy.

The default `subword-hash-v1` model is offline and reproducible. It improves identifier-shape and vocabulary discovery but is neither a learned code embedding nor compiler proof. Dependency and impact claims require typed edges with provenance and confidence. Ambiguous short names, comments, strings, dynamic dispatch candidates, generated code, runtime registration, and external consumers remain explicit uncertainty.

GTK `.ui`, Blueprint, CSS, XML/D-Bus, desktop/service, Cargo, and common configuration artifacts participate in lexical search and invalidation. Discoverability does not prove that a runtime can instantiate or register those artifacts.

## Change preparation and validation

`change.prepare` captures a freshness-labelled impact briefing before edits. It returns a task ID; after polling `task.get`, the completed result contains the context ID, likely change surface, source slices, semantic/static provenance, governing evidence, ambiguity, validation queue, and current architecture-finding baseline. `change.get` recovers that prepared evidence by context ID, and `repo.matrix` supplies the bounded Cargo feature/profile plan referenced by its verification guidance.

`change.validate` compares a diff with that prepared context and optionally runs checks. It is also task-backed and never refreshes first. Its architecture delta distinguishes new, worsened, resolved, and unchanged baseline findings and considers only files touched by the diff. Inferred architecture results remain advisory; only a separately human-approved quality constraint may block. When checks are requested, locally available Rust and artifact validators report explicit pass, failure, or unavailable evidence. Runtime registration, deployment behavior, and external compatibility remain separate obligations.

Choose one validation input: inline `git_diff`, a local UTF-8 `diff_path` (absolute or repository-relative), or `base_ref` with optional `target` (`HEAD` or `worktree`, default `worktree`). Git comparisons resolve commits locally and compare the base directly, without an implicit merge-base. `HEAD` scopes the patch to committed changes; `worktree` also includes staged and unstaged tracked changes. Omitting all inputs keeps pending tracked changes against HEAD (against the index in an unborn repository). Git comparisons exclude untracked files. Conflicting inputs, invalid refs, and unreadable patches fail explicitly. The report records `diff_scope` with resolved commits, patch bytes, and a BLAKE3 hash alongside `changed_files`; `analysis_target` remains `current_worktree`, because analysis and checks do not switch checkouts. A prepared baseline must still represent the pre-change source to provide a meaningful architecture delta; changing the diff input cannot reconstruct a missing baseline.

The minimal public quality loop exposes `problem.list/get/update`, `quality.list/get/review`, and `validation.queue/record`. Problem evidence can be completed or corrected, but activating a learned constraint requires an identified human reviewer and explicit confirmation. Constraint review history and validation outcomes are durable and inspectable. Historical constraints never create human work or expand implementation scope by themselves.

## Decisions and steering

Decisions and steerings live in `index.sqlite3` beside the derived tables but are never rebuilt; they are human records, and Crusty does not infer, merge, or expire them on its own. A decision is `accepted`, `superseded`, or `retired`, and unknown status values are rejected at write time because an unrecognised status used to make a decision permanently invisible to consultation with no diagnostic.

Only accepted decisions govern. `repo.consult`, `repo.constraints`, `repo.context`, `repo.explain`, `repo.authority`, and `change.prepare` all read the accepted set; superseded and retired decisions stay in the ledger as history. `decision.record` may list one or more accepted decisions in `supersedes`: each becomes `superseded`, the new record keeps the forward links, and `recorded_by` is required because another human's record changes. `decision.retire` moves an accepted decision to `retired` without a replacement and requires `retired_by`. Both transitions are terminal, reject unknown or already-closed targets, append a row to the decision's `history` with the action, actor, note, and time, and rewrite the `Status:` line of any markdown the decision materialized under `docs/decisions/`. Every decision exposes `supersedes`, `superseded_by`, and `history`, and `decision.list` enumerates the whole ledger newest first when no scope is given, applies its `limit`, and accepts a `status` filter.

`repo.consult` fills its sections in authority order until the token budget is spent. Its `context_budget` now reports `truncated` and per-section `omitted` counts so a starved section is visible; raise `budget` or call `repo.constraints` for the unbudgeted set. Steering keeps its lazy `expires_at` check, but the timestamp and the `active`/`retired` status are validated on write so a typo can no longer make a steering permanent.

Databases written before multi-target supersession stored one bare decision ID in `supersedes`; Crusty rewrites those rows into the list form on open, and the legacy memory mirror carries the same list.

## Research, findings, and work

`research.start` performs a bounded local scan and prepares primary-first web-search queries for the attached agent. `research.list` recovers run and task IDs, while `research.get` returns the run, task, budget consumption, packet, and resulting findings. Research does not invoke arbitrary external connectors. The separate [GitHub delivery adapter](github-delivery.md) uses the installed authenticated CLI under explicit policy. The attached agent performs `web_search` and submits qualified local, primary-web, or secondary-web evidence through `research.submit`.

Every finding is categorized as technical, product, or design and begins as `proposed`. Architecture audit results remain in their report unless `audit.finding.propose` explicitly copies one into this lifecycle with local evidence, counter-evidence, and limitations. `finding.review` records an append-only human decision. When the result is `needs_evidence`, `finding.evidence.add` can append qualified evidence and reopen the proposal without discarding review history. `finding.promote` can create project work only after acceptance and explicit human confirmation.

`work.list`, `work.get`, `work.recommend`, `work.create`, and `work.update` all use `memory.sqlite3`. `work.update` covers all human-editable create-time fields. `depends_on` records prerequisite work, while `blocked_by` records work that explicitly prevents progress; both contain exact work-item IDs. Create and update operations reject unknown, duplicate, self-referential, and cyclic relationships. Supplying a relationship list replaces it, an explicit empty list clears it, and omitting it during update preserves the stored value. Reads expose unresolved dependencies and blockers plus a derived `ready` flag. A relationship resolves when the referenced work reaches `completed` (or the historical `complete` spelling). Recommendations include only human-owned, accepted or active work with no unresolved relationships; the historical `in_progress` spelling remains readable. Crusty's ledger records repository-local intent; it does not replace GitHub issues or a human product backlog.

`memory.search` is the read-only recovery boundary for human-authored Codex prompts and preserved legacy decisions, steerings, problems, and quality constraints. It does not promote, infer, summarize, or mutate work. An attached agent must present recovered intent faithfully and use explicit human-confirmed `work.create` before it enters the queue.

## Migration from 0.1

Crusty 0.3 retains the clean-break MCP API introduced by 0.2. Exact old tool names in `AGENTS.md`, Codex approval rules, or automation must be updated. See the [Codex installation and migration guide](codex-installation.md) for the mapping.

No manual SQL migration is required. On first 0.2-or-newer open:

1. Crusty creates `memory.sqlite3`.
2. Legacy work is copied into the current work store.
3. Decisions, steerings, problems, and quality constraints are preserved as legacy records and synchronized again on later opens so post-migration guidance remains visible.
4. The old index database is left in place and is not deleted by the import.

Derived evidence is intentionally migrated separately. Because Crusty does not refresh implicitly, each existing project needs one explicit `index.refresh(scope="workspace")`, polled with `task.get`, to publish indexer version 8 and the `symbol-card-v1` backfill. Projects may do this lazily. Live exact search remains available before the refresh; broad results identify the previous generation as stale.

An incompatible or deliberately reset legacy schema may lose legacy memory, so deleting state is an explicit emergency action rather than a normal upgrade step.

## Compatibility boundaries

Crusty can establish what it observed in a particular live worktree or published snapshot. It cannot establish deletion safety, external API compatibility, runtime registration, generated-code consistency, deployment state, behavior under unindexed feature profiles, or product value without additional evidence.

The index, findings, and research output are guidance and provenance. Source, compiler/runtime results, external-system state, and human decisions remain authoritative.
