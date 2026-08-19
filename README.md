# rust-repo-intelligence

`rust-repo-intelligence` is a local, read-mostly MCP server that gives coding agents evidence-backed, snapshot-aware intelligence about a Rust workspace. It maps repository concepts to source symbols, dependencies, decisions, history, tests, and documented work while keeping the evidence provenance visible.

The server is deliberately local: it reads the workspace, invokes Cargo and Git as needed, stores its rebuildable cache beside the workspace, and communicates with the MCP client over JSON-RPC on standard input/output. It does not require a hosted database or a project-specific agent integration.

> Referenced code can still be architecturally obsolete.

The server only classifies a path as architecturally superseded when source or decision evidence names a replacement. A naming pattern such as `legacy_*` is a low-confidence suspected fallback—not proof that removal is safe.

## Architecture

The implementation is a single Rust package with two entry points:

- `src/main.rs` handles command-line parsing, the stdio JSON-RPC loop, MCP dispatch, and response formatting.
- `src/lib.rs` contains the `Service`, indexing pipeline, SQLite schema, rust-analyzer client, query methods, and tests.

The main components are:

1. **MCP adapter.** Reads one JSON-RPC request per line, dispatches MCP tool/resource calls, and emits one response per request. Notifications are processed without a response.
2. **Workspace service.** Resolves the workspace, combines bounded filesystem notifications with periodic Git reconciliation, selects a full or incremental refresh, and commits each derived-index generation atomically.
3. **Source indexer.** Parses valid Rust with `syn` (retaining a regex fallback for symbol discovery during temporarily invalid edits), includes module/trait/impl ownership in symbol identities, and collects references from the Rust AST rather than raw lines. Qualified paths and unique names become confidence-labelled impact edges; ambiguous short names are retained as unresolved evidence and never fanned out across every same-named symbol.
4. **Cargo integration.** Uses one `cargo metadata` snapshot per full refresh to record packages, dependency edges, feature sets, cfg/dependency-kind context, and Cargo-related refresh boundaries.
5. **Git integration.** Records recent commits, changed files, normalized file co-change relationships, and non-invasive checkpoints stored below `refs/codex/checkpoints/`.
6. **Artifact and work indexers.** Index Markdown/text, Cargo/configuration files, GTK `.ui`/Blueprint/CSS, XML (including D-Bus contracts), desktop/service files, infer test use cases, and turn TODO/FIXME markers into explicitly proposed work items.
7. **Semantic companion.** Optionally starts one long-lived `rust-analyzer` process when explicitly enabled. Reference and implementation answers are persisted as high-confidence edges under an identity containing the source/Cargo digest, lockfile, target, feature profile, build environment, and analyzer version.
8. **Hybrid retriever.** Builds deterministic symbol-card embeddings locally, performs brute-force cosine ranking, expands typed graph relationships according to query intent, and combines those channels with BM25 using reciprocal-rank fusion. `repo.context_pack` packs the result under an approximate token budget.
9. **Durable quality learning.** Records redacted bug evidence, deterministic defect families, reviewable scoped invariants, and snapshot-bound validation obligations. Only constraints explicitly made `active` with `approved` or `established` maturity can affect a later change; they add checks, never feature work.
10. **SQLite FTS, graph, vector, and memory store.** Stores the index and repository-scoped quality memory in `.rust-repo-intelligence/index.sqlite3`. WAL mode, foreign keys, atomic savepoints, explicit published generations, additive schema migrations, and extractor-version invalidation keep evidence consistent.

### Request and refresh flow

```text
MCP client
   │ JSON-RPC over stdin/stdout
   ▼
main.rs ──► Service::refresh_if_stale()
               │
               ├─ no watcher/HEAD changes: answer from the published generation
               ├─ watcher hints: debounce and refresh only affected inputs
               ├─ periodic reconciliation: verify against Git/content identities
               ├─ source changes: refresh affected symbols and derived edges
               └─ Cargo/toolchain/config changes: rebuild the semantic index
               │
               ├─ SQLite cache
               ├─ Cargo metadata / Git history
               └─ rust-analyzer LSP queries when semantic navigation is needed
```

The cache is never treated as authoritative for facts it cannot prove. Runtime registration, generated code, dynamic dispatch, external consumers, deployment state, and configuration-selected implementations remain explicit blind spots in responses.

## MCP tooling

- `repo.orient`
- `repo.context_pack`
- `repo.prepare_change`
- `repo.expand_context`
- `repo.history`
- `repo.checkpoint.create`, `repo.checkpoint.list`, `repo.checkpoint.diff`, `repo.checkpoint.restore_branch`
- `repo.record_decision`
- `repo.steering.record`, `repo.steering.list`
- `repo.validate_change`
- `repo.cleanup_candidates`
- `repo.locate`, `repo.explain`, `repo.why`, `repo.constraints`
- `repo.obsolete_candidates`
- `repo.work.list`, `repo.work.next`, `repo.work.propose`, `repo.work.update`
- `repo.problem.record`, `repo.problem.update`, `repo.problem.list`
- `repo.quality.propose`, `repo.quality.update`, `repo.quality.merge`, `repo.quality.list`, `repo.quality.explain`
- `repo.validation.queue`, `repo.validation.record`
- `repo.status`, `repo.refresh`
- `repo.matrix`

The intended change workflow is:

1. Call `repo.orient` to map an unfamiliar request to likely packages and symbols.
2. Call `repo.prepare_change` before editing. It creates a snapshot-bound context containing source slices, references, tests, decisions, lifecycle evidence, work items, risk notes, and any applicable learned validation obligations.
3. Use `repo.expand_context`, `repo.history`, `repo.explain`, or `repo.why` when more evidence is needed.
4. Make the code change in the client or working tree.
5. Call `repo.validate_change` with the prepared context and diff. It checks changed-file scope, expected callers, architecture conflicts, legacy paths, and recommended verification. With `run_checks=true`, it also executes deterministic queued recipes and passes changed `.ui`, `.blp`, and `.xml` files to `gtk4-builder-tool`, `blueprint-compiler`, or `xmllint` when installed. Manual or unavailable learned checks remain visible as `unavailable`; they are never silently treated as success.

Problem intake is a separate review loop. `repo.problem.record` redacts evidence, classifies the report, deduplicates it, and creates a `proposed` constraint. Use `repo.quality.update` to narrow or exclude scope, choose `observe`, `validate`, or `block`, and explicitly approve it by setting `status` to `active` and maturity to `approved` or `established`. Manual and visual-only recipes cannot be promoted to blocking. `repo.quality.explain` exposes both positive and negative match evidence, while `repo.validation.queue` separates repository policy, learned checks, and validators inferred from the current change.

The remaining tools support direct discovery (`locate`, `constraints`, `obsolete_candidates`), decision records, cleanup candidates, status/refresh operations, and editable work items. Work-queue and status reads use the durable SQLite cache directly; they do not trigger repository indexing or start the semantic companion. Run `repo.refresh` when automatically discovered work needs to be brought up to date.

## Run and configure

```bash
cargo run -- --workspace /path/to/rust/workspace
```

The service uses JSON-RPC over standard input/output. It writes its warm index and repository memory to `.rust-repo-intelligence/index.sqlite3` in the target workspace. SQLite runs in WAL mode with foreign keys enabled. The schema-v7 migration from v4-v6 is additive and preserves decisions, steerings, work items, and existing indexed state while creating the quality-memory tables. Incompatible schema resets remain an explicit emergency escape hatch. Add it to a Codex MCP configuration as a stdio command and follow the repository policy in `AGENTS.md`.

For a local build:

```bash
cargo build --release
./target/release/rust-repo-intelligence --workspace /path/to/rust/workspace
```

If `--workspace` is omitted, the current directory is used. The default index is static and provenance-labelled. Enable compiler-backed semantic navigation explicitly when the additional memory cost is acceptable:

```bash
RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER=1 cargo run -- --workspace /path/to/rust/workspace
```

If an explicitly enabled `rust-analyzer` cannot be started, the service continues with its syntax index and reports the attempted program plus the complete startup error in `repo.status`. Resolution prefers an explicit path, then `rustup which rust-analyzer`, then `PATH`:

```bash
RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH=/absolute/path/to/rust-analyzer \
RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER=1 \
cargo run -- --workspace /path/to/rust/workspace
```

Filesystem notifications are enabled by default and are used as bounded, debounced hints; database work remains on the service thread. A full Git/content reconciliation runs at startup and periodically to recover from missed or overflowed events. Disable notifications when a filesystem backend is unreliable:

```bash
RUST_REPO_INTELLIGENCE_ENABLE_WATCHER=0 cargo run -- --workspace /path/to/rust/workspace
```

Set `RUST_REPO_INTELLIGENCE_FEATURES` and/or `CARGO_BUILD_TARGET` when indexing a non-default semantic profile. Those values are part of semantic-cache identity, so cached compiler facts cannot silently cross profiles.

When enabled, the background analyzer is started lazily on the first index-backed request and configured conservatively: one rust-analyzer worker thread, one Cargo build job, no cache priming, and no editor-style check-on-save diagnostics. Work-queue access never starts it. The normal source indexer is synchronous and single-threaded as well.

The index records source locations, typed syntax/static/semantic edges, local symbol embeddings, Cargo package relationships with feature/cfg contexts, declared package targets and feature definitions, decision targets, Git commits and changed files, file co-change scores, lifecycle evidence, and provenance-labelled use-case/work links. Clean tracked files use Git blob identities; only dirty or untracked inputs are content-hashed. Input changes refresh affected source symbols and derived edges, while Cargo, lockfile, build-script, toolchain, Cargo-config, semantic-profile, schema, or extractor-version changes trigger an atomic full refresh. When enabled, `rust-analyzer` resolves references and implementations on demand and reuses them only within the matching semantic snapshot.

Checkpoint creation copies the real Git index into a temporary index, stages tracked worktree changes there, writes a tree and commit object, and updates a hidden checkpoint ref. It never checks out a commit or mutates the user's branch/index. Untracked files are excluded unless `include_untracked=true`; restoration creates a `codex/` branch for review rather than overwriting the current worktree.

## Evidence model and limitations

- `RustAnalyzer` locations are compiler-backed navigation evidence.
- Persisted `RustAnalyzer` edges represent resolved references or implementations. `Syntax` edges come from parsed AST paths: qualified and unique resolutions can inform impact, dynamic or ambiguous names cannot.
- Ambiguous syntax references are recorded separately, exposed in prepared change contexts/status, and excluded from `likely_change_surface`.
- GTK, Blueprint, CSS, XML/D-Bus, desktop, service, and common configuration artifacts participate in FTS. Searchability is evidence, not proof that runtime registration or external consumers are compatible.
- `SourceDoc` decisions/lifecycle entries quote source or authored documentation.
- `Git` entries are historical file-level evidence.
- `Heuristic` and `AgentInference` entries are never confirmed facts.
- Work discovered from TODO/FIXME comments is always `proposed`; explicit MCP proposals are attributed to the caller.
- Deterministic defect classification is a proposal source, not approval. Only reviewed, active learned constraints enter future validation queues, and each obligation retains the matching selectors, original problem links, recipe, enforcement, outcome evidence, and provenance.
- Problem reports, occurrences, recipes, and validation evidence are redacted before persistence. The built-in redactor removes common email addresses, bearer tokens, access-key/token formats, and secret assignments; callers should still avoid submitting unnecessary private message bodies.
- The current implementation explicitly reports blind spots for runtime registration, generated code, dynamic dispatch, external consumers, deployment state, and configuration-selected implementations. It does not claim safety from a missing static edge.

## Development and verification

The project is intentionally dependency-light: SQLite is bundled through `rusqlite`, and the remaining dependencies cover JSON, dates, hashing, regex scanning, and directory traversal. The standard verification commands are:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

The regression suite covers FTS and hybrid ranking, ambiguity-suppressed AST references, affected-file precision, GTK/D-Bus artifact retrieval, embeddings, typed structural edges, semantic-cache reuse and startup diagnostics, watcher input hints, generation publication, per-symbol caller attribution, atomic rollback, extractor-version backfill, Git history links, checkpoints, incremental refreshes, decisions, work items, validation diff parsing, lifecycle classification, and bounded rust-analyzer settings. A reproducible release-mode latency/recall/forbidden-hit harness is also available:

```bash
cargo run --release --example evaluate -- "$PWD"
```

## License

Licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).

## Roadmap

FTS5/vector/graph reciprocal-rank fusion, token-budgeted context packs, semantic-edge caching, Git/watcher-aware invalidation, explicit atomic generations, Git checkpoints, and the Cargo-declared feature/target matrix are implemented. `repo.matrix` now emits a bounded default/no-default/all-features/individual-feature verification plan; it does not pretend those commands were executed or cover every feature interaction. The local embedding model is deliberately deterministic and offline; replacing it with a learned code model is justified only when a broader evaluation corpus demonstrates enough recall gain to offset model downloads and cold-start cost. Target-specific matrix execution, deeper framework validators, contradiction-aware decision extraction, and broader multi-repository corpora remain future work.
# crusty
