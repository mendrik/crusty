# Evaluation baseline and current slice

This is a correctness-oriented evaluation, not a claim of universal repository understanding or token savings.

| Measurement | Baseline | Current slice |
| --- | ---: | ---: |
| Unit regression tests | 4 | 8 |
| MCP tools | 7 | 18 |
| Persistent lifecycle evidence | no | yes |
| Persistent editable work items | no | yes |
| Referenced-but-superseded regression | no | yes |
| Snapshot in repository-memory responses | partial | yes |
| Token-savings A/B benchmark | not measured | not measured |

## Reproducible evaluation fixture

`tests::finds_referenced_but_explicitly_superseded_compatibility_path` constructs this source graph:

```text
consumer ──calls──> legacy_store ──calls──> canonical_store
                         │
                         └── source documentation: “replaced with canonical_store”
```

Required result:

- `repo.obsolete_candidates(scope="legacy_store")` classifies `legacy_store` as `architecturally-superseded`;
- it retains `consumer` as a current caller rather than confusing “referenced” with “required”;
- it names `canonical_store` as the replacement;
- it reports a removal verification boundary and unresolved external-client question;
- the TODO becomes a `proposed`, `SourceDoc` work item, not an accepted plan.

Run the suite with:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

## Not yet measured

The project intentionally does not assert token, latency, affected-component recall, obsolete-code precision/recall, or false-safe rates beyond the fixture above. Those require a multi-repository corpus with ground truth. Before claiming those metrics, add fixtures for dynamic dispatch, feature gates, generated code, persistence, public/external API consumers, contradictory decisions, and multiple worktrees; then compare agent task runs with and without the MCP.
