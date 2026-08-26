# Crusty 0.2 state, retrieval, and migration

Crusty separates live navigation, rebuildable repository intelligence, and durable human/project memory. This separation is the core authority boundary: current source and runtime/compiler behavior remain authoritative, indexed relationships are freshness-labelled guidance, and autonomous findings remain proposals until a human acts on them.

## Authority and freshness

`repo.search(mode=exact)` reads Rust source from the live worktree and never refreshes the index. It is the preferred path for identifiers, call sites, and compiler-error navigation.

`repo.search(mode=broad)` and `repo.context` read the last atomically published index generation. Their responses include a freshness envelope containing the live and indexed revisions, generation, staleness, confidence, and an explicit note that no implicit refresh was attempted.

`index.status` reports that same boundary plus the latest refresh task and publisher lock. `index.refresh` is the only public refresh entry point. It returns a task ID immediately; `task.get` exposes queued, running, completed, or failed state. Readers keep using the previous generation while a single writer builds and publishes the replacement.

Cargo/toolchain/profile inputs or an indexer-version change force a full rebuild. Ordinary source changes may use incremental invalidation. The semantic snapshot includes the source/Cargo digest, `Cargo.lock`, target triple, feature profile, build-environment fingerprint, and rust-analyzer version.

## Storage

Repository-local state lives under the ignored `.rust-repo-intelligence/` directory:

- `index.sqlite3` contains rebuildable source, Cargo, Git, document, graph, search, embedding, and prepared-change projections.
- `memory.sqlite3` contains research runs, evidence, findings, task history, review decisions, and the single human-owned work store.
- `index.lock` is the filesystem publisher lease used to reject concurrent writers.

Both databases use SQLite WAL mode. Index publication is transactional, while durable-memory operations use short connections and a busy timeout. Indexing never modifies repository source files.

Codex's own session history remains owned by Codex and is not copied into either Crusty database. `memory.search` reads it on demand, uses session metadata to restrict results to the exact active repository, includes side-session records represented in the thread-history projection, excludes assistant/tool/injected-context content, and returns bounded matching user prompts with provenance. This is recovery evidence, not a new canonical roadmap.

## Hybrid retrieval

`repo.context` and broad search combine FTS5/BM25, deterministic local symbol-card embeddings, and intent-directed typed graph expansion. Reciprocal-rank fusion combines ranks without treating lexical and cosine scores as interchangeable. Exact symbol identities always precede non-exact fused candidates, and ranking ties and graph seeds are deterministic.

The embedding unit is `symbol-card-v1`, a bounded representation of canonical identity, module, kind, visibility, crate, file, source, and typed incoming/outgoing relationships. The card hash—not merely the source node hash—keys reuse. Incremental refresh moves unchanged vectors to the new semantic snapshot and recomputes changed cards. Status and evaluation output report the model, dimensions, card version, semantic snapshot, recomputed/reused counts, and storage strategy.

The default `subword-hash-v1` model is offline and reproducible. It improves identifier-shape and vocabulary discovery but is neither a learned code embedding nor compiler proof. Dependency and impact claims require typed edges with provenance and confidence. Ambiguous short names, comments, strings, dynamic dispatch candidates, generated code, runtime registration, and external consumers remain explicit uncertainty.

GTK `.ui`, Blueprint, CSS, XML/D-Bus, desktop/service, Cargo, and common configuration artifacts participate in lexical search and invalidation. Discoverability does not prove that a runtime can instantiate or register those artifacts.

## Change preparation and validation

`change.prepare` captures a freshness-labelled impact briefing before edits. It returns a task ID; after polling `task.get`, the completed result contains the context ID, likely change surface, source slices, semantic/static provenance, governing evidence, ambiguity, and validation queue.

`change.validate` compares a diff with that prepared context and optionally runs checks. It is also task-backed and never refreshes first. When checks are requested, locally available Rust and artifact validators report explicit pass, failure, or unavailable evidence. Runtime registration, deployment behavior, and external compatibility remain separate obligations.

Legacy approved quality constraints may still contribute validation evidence. Crusty 0.2 does not expose the old problem/quality administration API, and historical constraints never create human work or expand implementation scope by themselves.

## Research, findings, and work

`research.start` performs a bounded local scan and prepares primary-first web-search queries for the attached agent. Crusty itself has no GitHub, CI, analytics, telemetry, or arbitrary external connector. The attached agent performs `web_search` and submits qualified local, primary-web, or secondary-web evidence through `research.submit`.

Every finding is categorized as technical, product, or design and begins as `proposed`. `finding.review` records a human decision. `finding.promote` can create project work only after acceptance and explicit human confirmation.

`work.list`, `work.get`, `work.recommend`, `work.create`, and `work.update` all use `memory.sqlite3`. `depends_on` records prerequisite work, while `blocked_by` records work that explicitly prevents progress; both contain exact work-item IDs. Create and update operations reject unknown, duplicate, self-referential, and cyclic relationships. Supplying a relationship list replaces it, an explicit empty list clears it, and omitting it during update preserves the stored value. Reads expose unresolved dependencies and blockers plus a derived `ready` flag. A relationship resolves when the referenced work reaches `completed` (or the historical `complete` spelling). Recommendations include only human-owned, accepted or active work with no unresolved relationships; the historical `in_progress` spelling remains readable. Crusty's ledger records repository-local intent; it does not replace GitHub issues or a human product backlog.

`memory.search` is the read-only recovery boundary for human-authored Codex prompts and preserved legacy decisions, steerings, problems, and quality constraints. It does not promote, infer, summarize, or mutate work. An attached agent must present recovered intent faithfully and use explicit human-confirmed `work.create` before it enters the queue.

## Migration from 0.1

Crusty 0.2 is a clean-break MCP API. Exact old tool names in `AGENTS.md`, Codex approval rules, or automation must be updated. See the [Codex installation and migration guide](codex-installation.md) for the mapping.

No manual SQL migration is required. On first 0.2 open:

1. Crusty creates `memory.sqlite3`.
2. Legacy work is copied into the 0.2 work store.
3. Decisions, steerings, problems, and quality constraints are preserved as legacy records and synchronized again on later opens so post-migration guidance remains visible.
4. The old index database is left in place and is not deleted by the import.

Derived evidence is intentionally migrated separately. Because 0.2 does not refresh implicitly, each existing project needs one explicit `index.refresh(scope="workspace")`, polled with `task.get`, to publish indexer version 8 and the `symbol-card-v1` backfill. Projects may do this lazily. Live exact search remains available before the refresh; broad results identify the previous generation as stale.

An incompatible or deliberately reset legacy schema may lose legacy memory, so deleting state is an explicit emergency action rather than a normal upgrade step.

## Compatibility boundaries

Crusty can establish what it observed in a particular live worktree or published snapshot. It cannot establish deletion safety, external API compatibility, runtime registration, generated-code consistency, deployment state, behavior under unindexed feature profiles, or product value without additional evidence.

The index, findings, and research output are guidance and provenance. Source, compiler/runtime results, external-system state, and human decisions remain authoritative.
