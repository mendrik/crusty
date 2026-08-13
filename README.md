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
2. **Workspace service.** Resolves the workspace, checks input hashes, selects a full or incremental refresh, and exposes the repository-intelligence operations.
3. **Source indexer.** Scans Rust files for source symbols, builds conservative static reference candidates, captures lifecycle evidence, and records source slices on demand.
4. **Cargo integration.** Uses `cargo metadata` to record packages, workspace dependency edges, feature sets, cfg/dependency-kind context, and Cargo-related refresh boundaries.
5. **Git integration.** Records recent commits, changed files, and file co-change relationships when Git metadata is available.
6. **Document and work indexers.** Index Markdown/text documentation, infer test use cases, and turn TODO/FIXME markers into explicitly proposed work items.
7. **Semantic companion.** Starts one long-lived `rust-analyzer` process when available. Semantic reference locations are marked `RustAnalyzer`; static fallbacks are marked `StaticIndex` with lower confidence.
8. **SQLite cache.** Stores the index in `.rust-repo-intelligence/index.sqlite3`. WAL mode, foreign keys, schema versioning, and input hashes make the cache durable but disposable.

### Request and refresh flow

```text
MCP client
   │ JSON-RPC over stdin/stdout
   ▼
main.rs ──► Service::refresh_if_stale()
               │
               ├─ unchanged inputs: answer from SQLite
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
- `repo.prepare_change`
- `repo.expand_context`
- `repo.history`
- `repo.record_decision`
- `repo.validate_change`
- `repo.cleanup_candidates`
- `repo.locate`, `repo.explain`, `repo.why`, `repo.constraints`
- `repo.obsolete_candidates`
- `repo.work.list`, `repo.work.next`, `repo.work.propose`, `repo.work.update`
- `repo.status`, `repo.refresh`
- `repo.matrix`

The intended change workflow is:

1. Call `repo.orient` to map an unfamiliar request to likely packages and symbols.
2. Call `repo.prepare_change` before editing. It creates a snapshot-bound context containing source slices, references, tests, decisions, lifecycle evidence, work items, and risk notes.
3. Use `repo.expand_context`, `repo.history`, `repo.explain`, or `repo.why` when more evidence is needed.
4. Make the code change in the client or working tree.
5. Call `repo.validate_change` with the prepared context and diff. It checks changed-file scope, expected callers, architecture conflicts, legacy paths, and recommended verification.

The remaining tools support direct discovery (`locate`, `constraints`, `obsolete_candidates`), decision records, cleanup candidates, status/refresh operations, and editable work items.

## Run and configure

```bash
cargo run -- --workspace /path/to/rust/workspace
```

The service uses JSON-RPC over standard input/output. It writes its warm index to `.rust-repo-intelligence/index.sqlite3` in the target workspace. SQLite runs in WAL mode with foreign keys enabled. The index is a rebuildable cache: a schema-version change discards and recreates it from Cargo, source, Git, and decision material. Add it to a Codex MCP configuration as a stdio command and follow the repository policy in `AGENTS.md`.

For a local build:

```bash
cargo build --release
./target/release/rust-repo-intelligence --workspace /path/to/rust/workspace
```

If `--workspace` is omitted, the current directory is used. If `rust-analyzer` cannot be started, the service continues with its static, provenance-labelled index.

The background analyzer is configured conservatively: one rust-analyzer worker thread, one Cargo build job, no cache priming, and no editor-style check-on-save diagnostics. This keeps a repository query from consuming the machine's full CPU capacity. The normal source indexer is synchronous and single-threaded as well.

The index records source locations, static and rust-analyzer-backed reference/implementation edges, Cargo package relationships with feature/cfg contexts, declared package targets and feature definitions, decision targets, Git commits and changed files, file co-change scores, lifecycle evidence, and provenance-labelled use-case/work links. Source slices are read from the current file at query time rather than duplicated in SQLite. Input hashes refresh only changed source symbols and fallback edges; Cargo, lockfile, build-script, toolchain, or Cargo-config changes trigger a full semantic refresh. `rust-analyzer` remains the live semantic authority; if a full LSP reference response is unavailable, the service labels fallback results as `StaticIndex` with reduced confidence rather than presenting guesses as semantic facts.

## Evidence model and limitations

- `RustAnalyzer` locations are compiler-backed navigation evidence.
- Persisted `RustAnalyzer` edges represent resolved references or implementations; `StaticIndex` edges are conservative lexical candidates.
- `SourceDoc` decisions/lifecycle entries quote source or authored documentation.
- `Git` entries are historical file-level evidence.
- `Heuristic` and `AgentInference` entries are never confirmed facts.
- Work discovered from TODO/FIXME comments is always `proposed`; explicit MCP proposals are attributed to the caller.
- The current implementation explicitly reports blind spots for runtime registration, generated code, dynamic dispatch, external consumers, deployment state, and configuration-selected implementations. It does not claim safety from a missing static edge.

## Development and verification

The project is intentionally dependency-light: SQLite is bundled through `rusqlite`, and the remaining dependencies cover JSON, dates, hashing, regex scanning, and directory traversal. The standard verification commands are:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

The regression suite covers indexing, incremental refreshes, decisions, work items, validation diff parsing, lifecycle classification, and bounded rust-analyzer settings. This is correctness and behavior coverage, not a token-savings benchmark.

## License

Licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).

## Roadmap

The first two roadmap slices are implemented: rust-analyzer reference/implementation edge persistence and a Cargo-declared feature/target matrix exposed through `repo.matrix`. Full matrix evaluation across selected target triples and feature combinations, framework scanners for configuration/API/database/UI boundaries, decision-document extraction with contradiction handling, and multi-worktree isolation remain future work. Vectors/FTS remain deferred until retrieval quality measurements show a need.
# crusty
