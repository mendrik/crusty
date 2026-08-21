# Evaluation baseline and observatory slice

This is a correctness-oriented evaluation, not a claim of universal repository understanding or token savings.

| Measurement | Baseline | Current slice |
| --- | ---: | ---: |
| Unit regression tests | 4 | 56 |
| Public MCP tools | 7 | 24 clean-break tools |
| Persistent lifecycle evidence | no | yes |
| Persistent editable work items | no | yes |
| Referenced-but-superseded regression | no | yes |
| Snapshot in repository-memory responses | partial | yes |
| Grounded lexical recall harness | no | yes |
| Hybrid BM25/vector/graph context pack | no | yes |
| Semantic-cache and watcher regressions | no | yes |
| Warm-query p50/p95 harness | no | yes |
| Ambiguous-reference false-positive regressions | no | yes |
| GTK/D-Bus artifact retrieval regressions | no | yes |
| Forbidden-result evaluation signal | no | yes |
| Durable, reviewable quality learning | no | yes |
| Explainable per-change validation queue | no | yes |
| Dirty-worktree exact navigation | no | yes |
| Explicit async refresh/research tasks | no | yes |
| Single-writer publisher lease | no | yes |
| Separate durable project memory | no | yes |
| Human-gated finding promotion | no | yes |
| Authenticated loopback finding inbox | no | yes |

## Reproducible evaluation fixture

`tests::finds_referenced_but_explicitly_superseded_compatibility_path` constructs this source graph:

```text
consumer ──calls──> legacy_store ──calls──> canonical_store
                         │
                         └── source documentation: “replaced with canonical_store”
```

The retained legacy-engine regression requires:

- the internal obsolete-candidate engine classifies `legacy_store` as `architecturally-superseded`;
- it retains `consumer` as a current caller rather than confusing “referenced” with “required”;
- it names `canonical_store` as the replacement;
- it reports a removal verification boundary and unresolved external-client question;
- the TODO becomes a `proposed`, `SourceDoc` work item, not an accepted plan.

Run the suite with:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Run the optimized retrieval evaluation with:

```bash
cargo run --release --example evaluate -- "$PWD"
```

Run the observatory latency contract with:

```bash
cargo run --release --example observatory_evaluate -- "$PWD"
```

On 2026-08-21, the final release build on the development workstation measured live exact search at 11.51 ms p50 / 12.74 ms p95 and warm indexed context at 124.24 ms p50 / 132.35 ms p95. The enforced SLOs are 150 ms and 750 ms respectively. These values are local regression evidence, not cross-machine guarantees.

The checked-in `evaluation/queries.json` corpus asserts self-repository recall for refresh, FTS, checkpoint, decision, and Git-history concepts. Each case may also name forbidden unrelated symbols. The harness reports incremental-refresh time, symbol-card recompute/reuse counts, warm `locate` p50/p95, hybrid `context_pack` p50/p95, combined recall@10, and forbidden-hit rate@10. Results are environment-specific and should be compared on the same machine/build profile.

## Observatory failure regressions

The observatory suite proves that an unrefreshed dirty symbol is visible to exact search, exact work lookup and recommendation share one database, legacy work migrates without mutating the old index, a second publisher lease fails closed, research submission creates proposed findings without work, and promotion is blocked until human review. A stale-snapshot regression indexes a function at line 801, shortens the live file to one line, and verifies broad context returns stale labelled evidence rather than panicking on an invalid slice.

The dashboard transport test establishes a session through the one-time URL and verifies that an authenticated mutation without the CSRF header is rejected. A real MCP stdio handshake is also exercised during release verification to inspect the negotiated server identity and clean-break tool list.

## Not yet measured

The self-repository harness is a regression signal, not a universal performance claim. The deterministic subword embedding primarily captures identifier and vocabulary similarity; comparison against a learned code embedding remains an evaluation question. The focused fixtures prove that qualified calls do not fan out to same-named neighbors, comments/strings do not create call edges, ambiguous calls remain explicit uncertainty, and GTK/D-Bus artifacts are searchable. The quality-memory fixtures cover the twelve acceptance boundaries: GTK and compiler families, approval/activation, narrow visual scope, unrelated-change rejection, deduplication, match explanations, disable/obsolete behavior, redaction, v6 migration, existing MCP compatibility, and provenance-preserving validation history. Token savings, affected-component recall across unrelated projects, obsolete-code precision/recall, and false-safe rates still require a multi-repository corpus with ground truth. Future corpora should cover dynamic dispatch, feature gates, generated code, persistence, public/external API consumers, contradictory decisions, and multiple worktrees, then compare agent task runs with and without the MCP.
