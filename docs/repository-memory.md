# Repository-memory API and migration notes

## Snapshot contract

Every new repository-memory query returns `snapshot`, containing the canonical workspace path, branch, worktree digest, index timestamp/state, and Cargo feature/target assumptions. Call `repo.refresh` to deliberately rebuild the cache; normal calls use input-hash invalidation.

## Lifecycle evidence

`repo.obsolete_candidates` reports lifecycle relationships only when the target has explicit source/document evidence of replacement. It returns current indexed callers, replacement path, evidence, provenance/confidence, removal slice, verification boundary, and unresolved questions.

Classifications:

- `architecturally-superseded`: explicit source evidence names the canonical replacement.
- `suspected-stale-fallback`: lifecycle naming/comment evidence exists but a replacement is not proven.
- `private-unreferenced`: exposed by the existing conservative cleanup query.

None implies deletion safety. External consumers, generated code, runtime registration, configuration-selected implementations, and persisted historical formats must be checked separately.

## Work ledger

`repo.work.propose` persists an explicit, inspectable work item; `repo.work.update` changes its status/evidence/dependencies; `repo.work.list` and `repo.work.next` retrieve it. Source TODO/FIXME discovery creates only `proposed` work with `SourceDoc` provenance. If an automatically discovered marker disappears, its evidence is replaced with a stale-evidence notice and its confidence is lowered for review; it is not silently accepted or deleted. Work item JSON is stored in SQLite as a rebuildable cache and tied to the snapshot that last validated it.

## Compatibility

Existing tools and resource URIs remain unchanged. New schema version 3 is a cache migration: an older `.rust-repo-intelligence/index.sqlite3` is discarded and reconstructed from repository evidence. No target-repository files are modified by normal indexing or query operations.
