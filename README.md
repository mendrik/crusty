# rust-repo-intelligence

`rust-repo-intelligence` is a local stdio MCP server that gives coding agents an evidence-backed, snapshot-aware memory of a Rust workspace. It maintains a SQLite cache and uses a long-lived `rust-analyzer` process when available.

> Referenced code can still be architecturally obsolete.

The server only classifies a path as architecturally superseded when source or decision evidence names a replacement. A naming pattern such as `legacy_*` is a low-confidence suspected fallback—not proof that removal is safe.

## Current MCP surface

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

## Run

```bash
cargo run -- --workspace /path/to/rust/workspace
```

The service uses JSON-RPC over standard input/output. It writes its warm index to `.rust-repo-intelligence/index.sqlite3` in the target workspace. SQLite runs in WAL mode with foreign keys enabled. The index is a rebuildable cache: a schema-version change discards and recreates it from Cargo, source, Git, and decision material. Add it to a Codex MCP configuration as a stdio command and follow the repository policy in `AGENTS.md`.

The index records source locations, static reference candidates, Cargo package relationships with feature/cfg contexts, decision targets, Git commits and changed files, file co-change scores, lifecycle evidence, and provenance-labelled use-case/work links. Source slices are read from the current file at query time rather than duplicated in SQLite. Input hashes refresh only changed source symbols and fallback edges; Cargo, lockfile, build-script, toolchain, or Cargo-config changes trigger a full semantic refresh. `rust-analyzer` remains the live semantic authority; if a full LSP reference response is unavailable, the service labels fallback results as `StaticIndex` with reduced confidence rather than presenting guesses as semantic facts.

## Evidence and limitations

- `RustAnalyzer` locations are compiler-backed navigation evidence.
- `SourceDoc` decisions/lifecycle entries quote source or authored documentation.
- `Git` entries are historical file-level evidence.
- `Heuristic` and `AgentInference` entries are never confirmed facts.
- Work discovered from TODO/FIXME comments is always `proposed`; explicit MCP proposals are attributed to the caller.
- The current implementation explicitly reports blind spots for runtime registration, generated code, dynamic dispatch, external consumers, deployment state, and configuration-selected implementations. It does not claim safety from a missing static edge.

## Evaluation baseline and current result

Baseline before this slice: 4 unit tests, no lifecycle/work queries, and static unreferenced cleanup only. Current regression suite: 8 tests, including a referenced `legacy_store` adapter explicitly replaced by `canonical_store`; it must be reported as `architecturally-superseded`, retain its current caller, and include a removal verification boundary. This is a correctness regression check, not a token-savings benchmark; the server does not claim token savings until an A/B benchmark exists.

## Roadmap

Next high-value additions are rust-analyzer-backed impl/call graph persistence, feature/target matrix evaluation, framework scanners for configuration/API/database/UI boundaries, decision-document extraction with contradiction handling, and multi-worktree isolation. Vectors/FTS remain deferred until retrieval quality measurements show a need.
# crusty
