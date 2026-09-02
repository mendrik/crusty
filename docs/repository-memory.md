# Crusty 0.3 state, retrieval, and migration

Crusty separates live navigation, rebuildable repository intelligence, and durable human/project memory. This separation is the core authority boundary: current source and runtime/compiler behavior remain authoritative, indexed relationships are freshness-labelled guidance, and autonomous findings remain proposals until a human acts on them.

## Authority and freshness

`repo.search(mode=exact)` reads Rust source from the live worktree and never refreshes the index. It is the preferred path for identifiers, call sites, and compiler-error navigation.

`repo.search(mode=broad)` and `repo.context` read the last atomically published index generation. Their responses include a freshness envelope containing the live and indexed revisions, generation, staleness, confidence, and an explicit note that no implicit refresh was attempted.

`repo.architecture` reads current manifests and Rust syntax directly from the live worktree. Its versioned facts and contextual findings are labelled separately from the published relationship graph. `audit.start` runs the same bounded analysis as a durable background task and persists its completed report.

`index.status` reports the snapshot boundary plus the latest refresh task and publisher lock. `index.refresh` is the only public refresh entry point. It returns a task ID immediately; `task.list` recovers bounded task summaries after an interrupted session and `task.get` exposes one task's queued, running, completed, or failed state and full result. Readers keep using the previous generation while a single writer builds and publishes the replacement.

Cargo/toolchain/profile inputs or an indexer-version change force a full rebuild. Ordinary source changes may use incremental invalidation. The semantic snapshot includes the source/Cargo digest, `Cargo.lock`, target triple, feature profile, build-environment fingerprint, and rust-analyzer version.

## Storage

Repository-local state lives under the ignored `.rust-repo-intelligence/` directory:

- `index.sqlite3` contains rebuildable source, Cargo, Git, document, graph, search, embedding, and prepared-change projections, the newest 20 snapshot-scoped architecture audits, plus the retained problem/quality/validation engine.
- `memory.sqlite3` contains research runs, evidence, findings, task history, finding review decisions, the single human-owned work store, and searchable summaries of retained legacy guidance.
- `index.lock` is the filesystem publisher lease used to reject concurrent writers.

Both databases use SQLite WAL mode. Index publication is transactional, while durable-memory operations use short connections and a busy timeout. Indexing never modifies repository source files.

Codex's own session history remains owned by Codex and is not copied into either Crusty database. `memory.search` reads it on demand, uses session metadata to restrict results to the exact active repository, includes side-session records represented in the thread-history projection, excludes assistant/tool/injected-context content, and returns bounded matching user prompts with provenance. This is recovery evidence, not a new canonical roadmap.

## Hybrid retrieval

`repo.context` and broad search combine FTS5/BM25, deterministic local symbol-card embeddings, and intent-directed typed graph expansion. Reciprocal-rank fusion combines ranks without treating lexical and cosine scores as interchangeable. Exact symbol identities always precede non-exact fused candidates, and ranking ties and graph seeds are deterministic.

The embedding unit is `symbol-card-v1`, a bounded representation of canonical identity, module, kind, visibility, crate, file, source, and typed incoming/outgoing relationships. The card hash—not merely the source node hash—keys reuse. Incremental refresh moves unchanged vectors to the new semantic snapshot and recomputes changed cards. Status and evaluation output report the model, dimensions, card version, semantic snapshot, recomputed/reused counts, and storage strategy.

The default `subword-hash-v1` model is offline and reproducible. It improves identifier-shape and vocabulary discovery but is neither a learned code embedding nor compiler proof. Dependency and impact claims require typed edges with provenance and confidence. Ambiguous short names, comments, strings, dynamic dispatch candidates, generated code, runtime registration, and external consumers remain explicit uncertainty.

GTK `.ui`, Blueprint, CSS, XML/D-Bus, desktop/service, Cargo, and common configuration artifacts participate in lexical search and invalidation. Discoverability does not prove that a runtime can instantiate or register those artifacts.

## Change preparation and validation

`change.prepare` captures a freshness-labelled impact briefing before edits. It returns a task ID; after polling `task.get`, the completed result contains the context ID, likely change surface, source slices, semantic/static provenance, governing evidence, ambiguity, validation queue, and current architecture-finding baseline. `change.get` recovers that prepared evidence by context ID, and `repo.matrix` supplies the bounded Cargo feature/profile plan referenced by its verification guidance.

`change.validate` compares a diff with that prepared context and optionally runs checks. It is also task-backed and never refreshes first. Its architecture delta distinguishes new, worsened, resolved, and unchanged baseline findings and considers only files touched by the diff. Inferred architecture results remain advisory; only a separately human-approved quality constraint may block. When checks are requested, locally available Rust and artifact validators report explicit pass, failure, or unavailable evidence. Runtime registration, deployment behavior, and external compatibility remain separate obligations.

The minimal public quality loop exposes `problem.list/get/update`, `quality.list/get/review`, and `validation.queue/record`. Problem evidence can be completed or corrected, but activating a learned constraint requires an identified human reviewer and explicit confirmation. Constraint review history and validation outcomes are durable and inspectable. Historical constraints never create human work or expand implementation scope by themselves.

## Research, findings, and work

`research.start` performs a bounded local scan and prepares primary-first web-search queries for the attached agent. `research.list` recovers run and task IDs, while `research.get` returns the run, task, budget consumption, packet, and resulting findings. Crusty itself has no GitHub, CI, analytics, telemetry, or arbitrary external connector. The attached agent performs `web_search` and submits qualified local, primary-web, or secondary-web evidence through `research.submit`.

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
