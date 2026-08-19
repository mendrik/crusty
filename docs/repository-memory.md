# Repository-memory API and migration notes

## Snapshot contract

Every repository-memory query returns the indexed `snapshot`, containing the canonical workspace path, branch, worktree digest, published generation, semantic snapshot, index timestamp/state, extractor version, and Cargo feature/target assumptions. `repo.status` also returns `current_revision` and an explicit `stale` flag so live worktree state is never presented as if it were the indexed generation. Clean tracked inputs use Git blob identities; dirty and untracked inputs use content hashes.

Full and incremental refreshes are atomic SQLite generations. Filesystem events are bounded, debounced hints; startup and periodic Git/content reconciliation recover missed events. Cargo/toolchain/profile inputs or an extractor-version change force a complete derived-index rebuild; source and document changes update only their affected domains. `repo.refresh(scope="git")` updates history/co-change search data without falsely marking dirty source as indexed.

The semantic snapshot identity includes the source/Cargo digest, `Cargo.lock`, target triple, configured feature profile, selected build-environment fingerprint, and rust-analyzer version. On-demand rust-analyzer references and implementations are persisted only under that identity. Public API, trait, impl, macro, Cargo, target, or feature changes invalidate the broad semantic cache; private-source edits retain only edges whose indexed endpoints survived.

## Hybrid context retrieval

`repo.context_pack` combines FTS5/BM25, deterministic local symbol-card embeddings, and intent-directed typed graph expansion. Reciprocal-rank fusion combines ranks without pretending BM25 and cosine scores share a scale. Returned symbols name their contributing channels, and source/artifact evidence is packed under an approximate token budget.

The embedding model is `subword-hash-v1`: offline, reproducible, and inexpensive enough to brute-force at the current repository scale. It improves spelling, identifier-shape, and vocabulary overlap; it is not presented as compiler proof or as equivalent to a learned code model. Dependencies are established only by typed edges with provenance and confidence.

Impact traversal is intentionally narrower than search. Parsed qualified paths and uniquely resolvable names may become `Syntax` dependency edges. Ambiguous short names, dynamic dispatch candidates, comments, and string contents do not become affected-file recommendations. Ambiguous AST references are stored as unresolved evidence and returned separately by `repo.prepare_change`; they remain available for human/compiler follow-up without inflating `likely_change_surface`.

GTK `.ui`, Blueprint, CSS, XML/D-Bus, desktop/service, Cargo, and common configuration artifacts join the FTS document corpus and Git-aware invalidation set. This lets concept lookup surface markup and boundary contracts, but does not assert that GTK can instantiate the markup, a runtime registered a D-Bus interface, or an external consumer remains compatible.

When `repo.validate_change` runs checks, changed `.ui`, `.blp`, and `.xml` artifacts are sent to the corresponding locally installed validator (`gtk4-builder-tool`, `blueprint-compiler`, or `xmllint`). Each result is explicit, including unavailable-tool skips. These checks can catch local markup/schema failures; runtime registration, D-Bus consumer compatibility, and deployment behavior remain separate integration obligations.

## Lifecycle evidence

`repo.obsolete_candidates` reports lifecycle relationships only when the target has explicit source/document evidence of replacement. It returns current indexed callers, replacement path, evidence, provenance/confidence, removal slice, verification boundary, and unresolved questions.

Classifications:

- `architecturally-superseded`: explicit source evidence names the canonical replacement.
- `suspected-stale-fallback`: lifecycle naming/comment evidence exists but a replacement is not proven.
- `private-unreferenced`: exposed by the existing conservative cleanup query.

None implies deletion safety. External consumers, generated code, runtime registration, configuration-selected implementations, and persisted historical formats must be checked separately.

## Work ledger

`repo.work.propose` persists an explicit, inspectable work item; `repo.work.update` changes its status/evidence/dependencies; `repo.work.list` and `repo.work.next` retrieve it. Source TODO/FIXME discovery creates only `proposed` work with `SourceDoc` provenance. If an automatically discovered marker disappears, its evidence is replaced with a stale-evidence notice and its confidence is lowered for review; it is not silently accepted or deleted. Work item JSON is stored in SQLite as a rebuildable cache and tied to the snapshot that last validated it.

Automatic marker discovery accepts Rust comment lines and explicitly actionable Markdown/text forms; prose discussing TODO/FIXME behavior, regex definitions, SQL strings, and source fixtures are not treated as plans.

## Decisions and steerings

`repo.record_decision` captures rationale, consequences, scope, lifecycle status, and supersession. `repo.steering.record` captures a scoped instruction with priority, status, and optional expiry; `repo.steering.list` retrieves it through FTS5. Relevant active records are included in prepared change contexts and constraint queries. As an intentional emergency escape hatch, a deliberate schema reset may erase this memory when part of a product needs to restart from a clean conceptual state.

## Learned quality constraints

Quality learning uses three separate durable records in the existing repository SQLite store:

- A problem record preserves redacted report evidence, deterministic family classification, lifecycle status, semantic scope, reproduction/diagnostic information, diagnosis and fix references, related records, and provenance. Similar active reports share a stable fingerprint and add occurrences instead of creating noisy duplicates.
- A learned quality constraint is the reusable rule proposed from one or more problems. It has independent scope and exclusions, activation criteria, a typed recipe, enforcement, confidence, maturity, provenance, expiry/invalidation data, and validation history.
- A validation obligation is a snapshot-bound activation for one prepared change. It records the selectors that matched, why the rule was selected, the exact command or procedure and expected result, blocking policy, status, evidence, and linked constraint identifiers.

The deterministic classifier covers compiler and Clippy warnings, runtime/GTK diagnostics, action wiring, visual consistency, dynamic text/markup, migrations, performance, and test regressions. Classifier output is always `proposed`. A constraint affects later changes only after `repo.quality.update` makes it `active` and gives it `approved` or `established` maturity. Scope/exclusions are matched against crates, files, modules, symbols, components, Cargo relationships, concepts, widgets, CSS classes, design tokens, diagnostics, and configuration kinds. Every match returns the exact selectors and affected surfaces; a negative explanation states whether the constraint was inactive, excluded, or unrelated.

`repo.prepare_change` persists matching obligations and returns a queue split into repository policy, checks learned from previous defects, and native validators inferred for the pending artifacts. Identical learned recipes share one obligation, and exact duplicates are omitted from the repository-policy section. Focused/structural checks are ordered before broad Cargo/runtime checks. `repo.validate_change(run_checks=true)` records deterministic learned and inferred outcomes; missing tools and manual procedures remain `unavailable` with evidence. Historical constraints add validation only and never expand implementation scope or create work items.

The MCP lifecycle is exposed through `repo.problem.record/update/list`, `repo.quality.propose/update/merge/list/explain`, and `repo.validation.queue/record`. The matching resources are `rustrepo://problem/{id}`, `rustrepo://quality/{id}`, and `rustrepo://validation/{context_id}`. IDs are stable within the repository (`PRB-####`, `QLT-####`, and content-derived `OBL-*`).

Schema v7 adds quality-memory and obligation tables additively when opening v4-v6 databases. Records survive MCP restarts and are included in FTS. An unknown/incompatible schema may still use the documented emergency reset path, which intentionally removes quality memory along with the other project memory.

## Git checkpoints

`repo.checkpoint.create` captures the current index plus tracked worktree changes in a temporary Git index and stores the resulting commit under `refs/codex/checkpoints/<label>/<id>`. It does not modify the real index, branch, or working tree. Untracked files require explicit opt-in. `repo.checkpoint.list` and `repo.checkpoint.diff` inspect checkpoints; `repo.checkpoint.restore_branch` creates a reviewable `codex/` branch without checking it out.

Checkpoints are recovery points, not automatic commits on the product branch. They can be expired by deleting their refs when repository policy permits.

## Compatibility

Existing tools and resource URIs remain unchanged. A schema-version reset may deliberately discard repository memory when a project needs an extreme clean start; extractor-version changes normally rebuild derived evidence in place without requiring a schema reset. Normal indexing/query operations modify only the ignored cache, while checkpoint calls explicitly add or inspect hidden Git refs.
