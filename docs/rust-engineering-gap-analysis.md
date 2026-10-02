# Rust engineering gap analysis

Status: proposed research for human review. Date: 2026-10-01. These findings do not activate repository decisions, work items, or quality rules.

This records the baseline audit before the owner's subsequent implementation request. Descriptions of missing capabilities and source line numbers below refer to that baseline. See the [implementation plan and current evidence](implementation-plan.md) and [engineering workflow](engineering-workflow.md) for the resulting implementation and remaining gaps.

## Assessment

Crusty has a credible foundation for a shared Rust engineering intelligence service: repository governance, bounded retrieval, durable change context, syntax and optional compiler-backed relationships, contextual architecture findings, and reviewable quality memory. It can already reduce rediscovery and preserve project intent.

It does not yet supply the engineering process and evidence needed to replace `codebase-kitchen-cleanup`, `super-rust-engineer`, and `rustitect`. Its largest gaps are reliable live semantics, project-specific verification, contextual engineering guidance, and measured outcomes. More syntax detectors or a larger instruction string alone would not close these gaps.

The practical long-term objective is for an LLM to obtain its relevant project facts, Rust guidance, verification plan, and evidence through Crusty. The compiler, tests, profilers, dependency sources, and human requirements still determine whether the result is correct and useful. Crusty should organize and expose their evidence; no tool can guarantee universally optimal code independently of workload and requirements.

## Scope and evidence

- Inspected current source at commit `21af7a48a988d004c1fcf5ba4e9829e8045fd075` (2026-09-30). The worktree was clean at the beginning of the review.
- Called `repo.consult` first and used `repo.context`, exact search, and `symbol.relations`. Applied accepted decision `DEC-0001`: broad discovery, precision-first impact, and explicit ambiguity.
- The attached server served generation 48, published 2026-09-02, at indexed commit `a4c2e4c05ca68ca65f7bfe3336057b725e8b042d`; it explicitly reported staleness. Current source takes precedence over that snapshot. No refresh was requested.
- Compared the local cleanup skill and complete playbook, Rust engineering skill and relevant ownership/concurrency/unsafe/testing/performance references, and `/home/mendrik/.codex/skills/rustitect/SKILL.md`.
- Checked official upstream tooling through the research skill; citations and qualifications appear below.
- Ran `cargo test --locked --offline --lib`: **128 passed, 0 failed**. Also ran the focused `rust_analyzer` filter: **5 passed**. These are existing regression tests, not an evaluation of generated-code quality or a real-analyzer cross-project benchmark. Binary/MCP integration tests and release latency harnesses were not run.

## What exists already

| Area | Current contribution | Limit |
| --- | --- | --- |
| Governance | Consultation, decisions, steering, problems, learned constraints, explicit human review | Strong authority boundaries; retrieved documents are snapshot-derived |
| Navigation | Live fixed-string Rust search, hybrid concept retrieval, indexed definitions/callers, optional analyzer references/implementations | Live text and semantic identity are separate; analyzer scope is narrow |
| Change workflow | Durable prepare/validate tasks, diff scope, impact candidates, architecture baseline/delta, explicit verdict | Verification is largely a fixed built-in command sequence |
| Architecture | Contextual detectors with evidence, counter-evidence, limitations, and advisory findings | Mostly syntax/path heuristics; no persistent domain ownership model |
| Cleanup | Unreferenced-private candidates, explicit supersession, current callers, proposed removal slice | No semantic duplication or complete dependency/configuration migration model |
| Performance | Crusty's own retrieval latency harness; performance-regression quality family | Observed projects get a manual benchmark procedure, not structured measurements |
| Recovery | Durable task/context recovery, checkpoints, human work, project prompt search | Useful memory infrastructure, rather than a full Rust engineering knowledge service |

Sources: [public API](../README.md), [state and authority model](repository-memory.md), [architecture analysis](../src/analysis.rs), [quality recipes](../src/quality.rs), [retrieval evaluation](evaluation.md).

## Priority gaps and concrete improvements

### P0: Make live semantic queries reliable and independent of index publication

**Evidence.** During this audit, `repo.search(mode=exact, query="RustAnalyzerClient")` located the struct at `src/lib.rs:341`. `symbol.relations(relation=definition)` returned its old indexed location at `src/lib.rs:288`, along with several members and an unrelated containing service. The wrapper correctly labels its published snapshot as stale; nevertheless, an agent still has to repair navigation outside the tool. Definition uses lexical `search_nodes`, and semantic locations require an active semantic snapshot and indexed target nodes. New or substantially moved symbols are not first-class semantic inputs. See [relationship resolution](../src/lib.rs) at lines 2526 and 4016 and [wrapper](../src/observatory.rs) at line 954.

**Recommended ownership.** A semantic query service should accept a live file/position or an exact, disambiguated identity. The published index remains the owner of broad discovery and historical graph evidence. File/position queries should work before the first refresh and after edits.

**Acceptance.** On an unindexed fixture, and after inserting lines/renaming a function without refreshing, definitions, references, and implementations resolve the current symbol. Ambiguous names return candidate identities requiring selection. Stale indexed locations remain explicitly historical.

### P0: Make analysis environment and query completeness observable

**Evidence.** `RustAnalyzerClient` sends empty client capabilities, discards notifications without IDs, and flattens request timeout/error/non-array results to `None`. Startup failures are exposed, but per-query readiness, errors, unsupported capabilities, and incomplete scope are not distinguished from fallback/absence. `did_change` ignores write failures. Semantic output is capped to eight targets and thirty locations per target; query-local completeness is not exposed. Paths are constructed and decoded with raw `file://` string operations. See [LSP transport](../src/lib.rs) at lines 349–529 and semantic conversion at line 4016.

The snapshot fingerprints features/target and some environment variables, which is valuable. However, `RUST_REPO_INTELLIGENCE_FEATURES` is read only for labels/identity; it is not wired into analyzer `cargo.features` or validation arguments. Initialization also lacks an explicit selected target/feature profile. Fingerprinting an intended profile does not establish that a tool used it. Rustc version is not a field in `SemanticSnapshot`. See snapshot construction at line 747 and [profile label](../src/analysis.rs) at line 1626.

**Recommended ownership.** One typed analysis configuration should drive actual tool invocation and result identity: toolchain, features/default-feature policy, target, cfgs, build-script/proc-macro state, and relevant environment. The LSP session should own capability negotiation, document synchronization, health/readiness, request lifecycle, URI conversion, and bounded results.

**Acceptance.** Tests distinguish disabled, unavailable, loading, failed, unsupported, complete-empty, and truncated answers. A nondefault feature changes both analyzer behavior and reported identity. Paths with spaces/non-ASCII characters round-trip. Every negative result states its covered configuration and scope. Use a small real rust-analyzer corpus as well as transport fakes.

### P1: Expose the LSP capabilities that improve code generation and refactoring

**Evidence.** The client currently requests only `textDocument/references` and `textDocument/implementation`. `definition` is indexed; `callers` returns `CALLS_DIRECT` graph edges, rather than LSP incoming call hierarchy. There is no exposed hover/type/signature query, dependency-definition lookup, macro expansion, diagnostic stream, rename, or code-action interface. Semantic locations outside the repository root are discarded. See [LSP methods](../src/lib.rs) at lines 446–476, relationship routing at line 2526, and root filtering at line 4080.

**Recommended sequence.** Add position-based hover/type/doc and real definition/type-definition first, including bounded external dependency source with package/version provenance. Then add incoming/outgoing call hierarchy and macro expansion. Add proposed rename/code-action edits with document versions, base hashes, conflict reporting, and normal prepare/validate integration. Completion and inlay hints are secondary unless agent evaluations show value.

**Acceptance.** An LLM can establish which locked dependency API exists, what an inferred expression type is, why a macro-generated item exists, and which exact symbol a rename affects without guessing from text. LSP edits are proposals until applied under the user's authorized task. Experimental analyzer extensions are capability/version checked. See the upstream capability table below.

### P1: Replace fixed validation with a project-derived verification plan

**Evidence.** `validate_change_from_source` always runs `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets` when checks are requested. This is not necessarily workspace-wide, does not follow every project's warning policy, and does not explicitly run doctests, supported targets, MSRV, or selected features. `repo.matrix` unconditionally recommends `--all-features` and up to twelve individual features, with a limitation disclaimer; it does not establish which combinations are valid. See [validation](../src/lib.rs) at line 3130 and matrix at line 3722. Human-approved custom Cargo recipes provide an extension path, but do not make the built-in plan adaptive.

**Recommended ownership.** One verification planner should combine Cargo topology, current CI/configuration, project promises, changed contracts, and approved recipes. Start with focused checks and expand to the supported workspace/profile matrix. Record the reason and exact configuration for each check. Explicitly mark promised checks that are unavailable or uncovered. Do not universally impose `-D warnings` or `--all-features`.

**Acceptance.** Fixtures cover a virtual workspace, a crate with mutually exclusive features, an MSRV promise, a doctest regression, and a target-specific API. The plan selects valid commands and identifies missing coverage. Separate overall required verification from the narrower verdict for executed checks; existing task-completion/verdict separation should remain.

### P1: Preserve compiler explanations and bind results to the code they checked

**Evidence.** Cargo JSON diagnostics already exist, which is a meaningful improvement. `cargo_diagnostics` retains level, code, message, and the first primary location, but drops secondary spans, child notes/help, macro origins, suggestions/applicability, and package/target identity. These often explain ownership and lifetime failures. See [diagnostic extraction](../src/lib.rs) at line 5814.

Reports record diff hash and revisions, and quality obligations are requeued on validation. Checks still read the mutable current worktree, including when the requested diff target is `HEAD`; this is documented. There is no explicit per-check pre/post source digest confirming that later edits did not invalidate a result. See [diff contract](repository-memory.md) and validation at line 3056.

**Recommended ownership.** A shared check-result model should preserve bounded diagnostic structure, command/toolchain/profile, exit status, test outcome, source digest, and truncation. Large details can be fetched by diagnostic ID. Check freshness should be invalidated when the relevant source changes during or after execution.

**Acceptance.** A borrow-checker diagnostic retains the originating borrow and conflicting use plus explanatory notes. A concurrent edit marks prior results outdated. Reports state precisely which revision/configuration passed, which checks failed, and which requirements remain unverified.

### P1: Turn skill expertise into task-specific engineering guidance

**Evidence.** The current server instructions and `AGENTS.md` mainly specify consultation, tool routing, preparation, validation, freshness, and human ownership. They do not supply the design/diagnosis workflow from the three compared skills. Consultation retrieves governance and documents but has no versioned, conditional Rust engineering guidance registry. See [server instructions](../src/main.rs) at line 897 and [repository policy](../AGENTS.md).

**Recommended ownership.** Keep a short portable agent contract and serve bounded engineering guidance selected by intent, code facts, and project policy. Each guidance item should carry applicability, rationale, exceptions, primary sources, version, and a verification method. Repository and user decisions retain precedence; inferred guidance never silently becomes an enforced constraint.

**Required topics.** Ownership from lifecycle; validated boundary types and enum states; error identity; minimal traits/visibility; cancellation/backpressure/idempotency; unsafe proof obligations; Cargo/MSRV/feature/API promises; tests derived from requirements; canonical ownership and complete removal; measured optimization. Include the skills' exceptions: cloning, `unwrap`, `anyhow`, traits, and large modules can be appropriate. Pattern presence is a lead, not a verdict.

**Acceptance.** An unsafe change gets invariant tracing and suitable verification, a performance request gets workload/baseline guidance, and a small isolated edit gets a short plan. Different MCP clients receive the same relevant intelligence without installing three separate skills or a giant all-purpose prompt. Client-side compliance still needs evaluation; MCP instructions cannot intercept arbitrary edits.

### P2: Represent domain ownership and architectural contracts explicitly

**Evidence.** Architecture facts/findings and before/after deltas already exist. Current analysis parses source with `syn`; roles are inferred from path tokens and dependency names. Rules find local safety-comment absence, spawn/lock patterns, boolean clusters, opaque errors, string IDs, configuration scatter, and serialization coupling. They do not reconstruct trusted ownership/invariant boundaries or prove behavior across calls. Presence of `JoinSet` or `FuturesUnordered` anywhere in the function suppresses an unbounded-spawn lead even though those types alone do not establish a concurrency limit. An environment-derived profile label does not mean `syn` filtered inactive cfg branches. See [detectors](../src/analysis.rs) at lines 497, 1369, 1430, and 1638.

**Recommended ownership.** Extend existing decisions and architecture facts with an explicit model of concepts, canonical owners, public/private operations, invariant-establishing constructors, mutation paths, representation conversions, permitted dependencies, and state/failure transitions. Resolve aliases/types through semantic evidence where useful. Proposed agent analyses should explain evidence and counter-evidence; only humans approve persistent architectural intent.

**Acceptance.** Crusty can explain why two modules violate the same approved invariant, how a public constructor bypasses validation, or where dependency direction changed. It should not recommend newtypes, traits, or layers just because a regex matched. Evaluate false positives and false negatives, including unrelated symbols with familiar names.

### P2: Model complete cleanup and migration slices

**Evidence.** Cleanup candidates are private indexed symbols with no inbound edges; obsolete candidates use lifecycle/replacement evidence and report callers/removal boundaries. This is useful but narrower than the cleanup skill's semantic consolidation. See [cleanup](../src/lib.rs) at line 3225 and lifecycle candidates at line 3519.

**Recommended ownership.** A migration plan should associate a proposed canonical concept with all legitimate callers, tests, manifests, configuration, generated artifacts, persisted formats, and concrete external contracts. Add evidence-backed semantic duplication, forwarding-only layers, redundant representations, unused dependency/feature checks, and follow-up removal sweeps. Keep similarity-based proposals distinct from compiler-confirmed dependencies.

**Acceptance.** A fixture with old/new implementations and a live configuration-selected caller yields one complete migration plan and identifies obsolete adapters, flags, tests, and dependencies. Public/external reachability remains an explicit boundary. The absence of an indexed edge never certifies deletion safety.

### P2: Add structured performance and specialized correctness evidence

**Evidence.** The performance-regression recipe is currently manual with no configured command or structured baseline. Crusty's release harness measures its own search/context latency, not the performance of code it advises agents to write. Safety detection checks local contract text and recommends Miri; it does not establish aliasing/provenance/layout invariants. See [quality families and recipes](../src/quality.rs) at lines 84 and 1572, [safety detector](../src/analysis.rs) at line 1080, and [latency harness](../examples/observatory_evaluate.rs).

**Recommended ownership.** Introduce typed workload/budget/baseline/measurement records linked to a revision and environment. Reuse approved project benchmarks/profilers; preserve distributions, input scale, allocations/memory, contention, binary/build cost, and tradeoffs. Separately support project-appropriate compile-fail/property/fuzz tests, Miri, concurrency modeling, ABI checks, and public API/semver/dependency checks through suitable adapters or evidence import. Existing approved Cargo recipes can run some commands; richer result models and non-Cargo tooling need additional integration.

**Acceptance.** A claimed optimization has comparable before/after evidence and retained correctness checks. Complexity concerns identify workload/scale assumptions. Unsafe and concurrency reports carry the invariant or behavioral oracle and execution limits; a successful check never means universal proof.

### P0 alongside development: Evaluate whether agents actually produce better Rust

**Evidence.** `evaluation/queries.json` has five self-repository queries. The harness measures discovery/recall/forbidden hits and latency; it does not measure completed agent engineering tasks. `docs/evaluation.md` explicitly lists multi-repository and false-safe evaluation as future work. Existing tests are valuable regression evidence, but cannot establish skill replacement.

**Recommended ownership.** Maintain a versioned multi-repository task corpus with expert-reviewed requirements and independent test/benchmark oracles. Compare the same agents/model versions under: ordinary tools, current skills, Crusty alone, and Crusty plus skills. Use repeated controlled runs and track correctness, supported-profile coverage, architecture regressions, measured performance, unsafe reasoning, patch scope, unnecessary dependencies, latency/cost, and actual model tokens.

**Acceptance.** Include tasks involving borrowing, trait/API design, macros, features, cancellation, persistence, FFI, duplication removal, stale state, and realistic optimization. For the skills Crusty aims to replace, predeclare acceptable outcome thresholds and verify them across several clients/models. Query recall and tool count are supporting metrics; they are not the success criterion.

## Skill replacement map

| Skill | Preserve as guidance | Add as Crusty evidence/planning capability | Current coverage |
| --- | --- | --- | --- |
| `super-rust-engineer` | Ownership/API judgment, diagnostic causality, risk-based verification, evidence-based performance | Live types/versioned API docs, complete diagnostics, supported check matrix, benchmark/specialized check evidence | Partial navigation, Cargo facts, checks, quality memory |
| `rustitect` | Semantic ownership, dependency direction, invariants, failure/cancellation/retry analysis, minimal architecture | Approved domain/ownership model, resolved cross-boundary evidence, invariant and state/mutation traces | Contextual syntax guard already implements a useful subset |
| `codebase-kitchen-cleanup` | Canonical owner selection, delete/reuse preference, complete migration, structural complexity review | Semantic duplication proposals, complete removal slices, manifest/config/test sweeps, measured scaling evidence | Dead/private and superseded-code candidates, callers/history |

Guidance, deterministic facts, and experimental checks have different owners. Do not encode every skill sentence as a lint, turn every observation into blocking work, or treat model recommendations as human decisions.

## Proposed implementation sequence

1. **Reliability and project contract.** Add live identity/position resolution, typed query status/configuration, fresh authoritative instruction reads, and a project-derived verification plan. In parallel, create the first cross-project agent evaluation tasks. Reuse the existing LSP backend, check runner, decision/quality stores, and task lifecycle.
2. **Semantic assistance and guidance.** Add types/docs/dependency lookup, full diagnostic explanations, call hierarchy/macros, and conditional engineering guidance. Then expose proposed rename/assist edits with freshness/conflict checks.
3. **Architecture and cleanup.** Extend current architecture facts with approved ownership/contracts, and build complete consolidation/removal plans around real evidence. Validate detector precision against different layouts and frameworks.
4. **Measured engineering.** Add performance contracts and specialized verification evidence, then use agent outcome results to determine which skill workflows Crusty can reliably subsume.

Keep the public interface organized around questions an agent must answer. The current 62-tool surface already has extensive memory/lifecycle operations; a coherent semantic query/result model is more useful than an unrestricted raw LSP passthrough or one new MCP tool per checklist item.

## Agent instruction changes proposed for review

The existing first-call, human ownership, and preparation/validation rules should remain. Add an explicit engineering loop:

1. Obtain current governance and the effective project contract; distinguish active instructions from historical snapshots.
2. Establish the task's observable acceptance criterion and relevant source/profile scope.
3. Retrieve the canonical owners, live semantic evidence, contracts, and conditional Rust guidance; resolve uncertainty before relying on negative results.
4. For design changes, state ownership, invariants, representation boundaries, failure/cancellation behavior, and justified performance assumptions.
5. Prepare; perform the smallest complete authorized change; migrate legitimate callers and remove obsolete paths within scope.
6. Execute the project-derived verification plan; link evidence to the actual revision/configuration and state uncovered requirements.
7. Review the result against the task and its invariants. Claim optimization only with comparable measurements. Keep new architectural/quality lessons proposed until reviewed.

This proposal belongs in Crusty's capability/guidance design and client contract, with bounded task-specific output. It is not an instruction to load every engineering topic for every request.

## Smaller instruction and documentation defects

- The installation guide still claims a 61-tool surface while the live contract test lists 62 tools; it also says decisions/steering/checkpoints stay out of public mutation despite documenting those public tools elsewhere. Generate capability/version facts from the actual API. See [installation guide](codex-installation.md) and [contract test](../src/main.rs) at line 909.
- The evaluation document's 116 regression-test count is behind the current 128 library tests. Counts should be dated observations or generated evidence.
- A docs-only preparation in this audit selected unrelated Rust symbols when the explicit path had no indexed match. This follows the intent-based fallback at `src/lib.rs:2410`. Preserve requested file scope; return discovery suggestions separately from the actual prepared change surface.
- `repo.search(mode=exact)` limits searches to `*.rs`. Offer bounded exact navigation for manifests/instructions/configuration too, since those define code behavior and verification policy.
- Consultation includes excerpts of published governing documents. Staleness is labelled, but a general client needs a supported way to retrieve current full governing instructions, including applicable nested instruction files, before acting. The supplied live `AGENTS.md` governed this session.

## Documentation workflow record

Prepared context: `ctx_5a3ac5ac5af8`, task `task_610a5f3a0cd6c396`, generation 48 with `stale=true` and the indexed/live revisions above. The research section was created while preparation was being captured; no Rust source was modified. The complete documentation patch is submitted separately to `change.validate`, since a Git comparison alone excludes this new untracked file. Documentation checks verify references and scope; no findings or requirements are activated by this report.

## Primary-source research: LSP utility and evidence limits

The distinctions below describe available upstream capabilities and proposed uses. They do not establish which capabilities Crusty currently implements; that requires local source evidence.

| Capability | What it gives an LLM | Proposed integration requirement |
| --- | --- | --- |
| Hover, type definition, documentation, inlay hints | Expression types, symbol documentation, inferred local types, and optionally lifetimes/reborrows | Return source positions, exact queried expression, and active analysis configuration with the answer. |
| Definition, references, implementations, rename | Resolve symbol identity, inspect implementations, trace uses, and produce semantic rename edits | Preserve result scope and exclusions; validate changed code after edits. |
| Incoming/outgoing call hierarchy | Position-based callers/callees with call-site ranges | Negotiate server capabilities and distinguish unsupported results from empty results. |
| Macro expansion, related tests | Explain generated code and identify tests using an item | Expose useful extensions through small, structured tools. |
| Code actions | Context-sensitive local transformations | Return proposed edits for review and subsequent validation. |

rust-analyzer documents hover/types, navigation, references including macro expansions, optional lifetime/reborrow hints, and semantic rename. These answer concrete semantic questions without asking the model to infer everything from text. [rust-analyzer features](https://rust-analyzer.github.io/book/features.html)

The standard call-hierarchy protocol separates preparation from incoming/outgoing queries and returns source ranges. Clients issue these queries when the server registers the capability. Proposed interpretation: this is navigation evidence, not a complete model of every runtime dispatch or execution. [LSP call hierarchy specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_prepareCallHierarchy)

rust-analyzer additionally documents `rust-analyzer/expandMacro`, `rust-analyzer/relatedTests`, expression-range hover, workspace reload, and server-status notifications. Status includes health and whether background work is pending; warnings/errors can accompany incomplete or incorrect answers. Proposed integration: attach health and synchronization evidence instead of silently presenting every result as equally trustworthy. Experimental extensions require version/capability handling. [rust-analyzer LSP extensions](https://rust-analyzer.github.io/book/contributing/lsp-extensions.html)

Assists are documented as small local refactorings available in context. Proposed interpretation: successful application does not establish whole-repository migration completeness, external compatibility, domain correctness, or an architectural improvement. [rust-analyzer assists](https://rust-analyzer.github.io/book/assists.html)

### The analysis environment is part of the answer

rust-analyzer has separate analysis/check configuration for features, default features, targets, environment variables, build scripts, procedural macros, and check commands. Its defaults also enable `cfg(test)` for local crates. Reference searches/call hierarchy can exclude tests. Proposed requirement: record toolchain/server versions, active features, target triple, relevant cfgs, build-script/proc-macro state, reference exclusions, and workspace health. A negative query should say what environment it covered; one configuration cannot certify the supported feature/target matrix. [rust-analyzer configuration](https://rust-analyzer.github.io/book/configuration)

### Compiler evidence must retain the explanation

Cargo's JSON stream distinguishes compiler messages, artifacts, build-script outputs, and build completion. Compiler messages identify package/target and embed rustc diagnostics. Crucially, `build-finished.success` reports compilation: tests or a launched program can still produce output afterwards. Proposed requirement: retain the command, exit status, build outcome, and runtime/test outcome separately. Arbitrary proc-macro/program output can coexist with JSON, so parsers must tolerate non-message lines. [Cargo external-tools interface](https://doc.rust-lang.org/cargo/reference/external-tools.html)

rustc diagnostic JSON includes primary/secondary spans, child notes/help, macro-expansion origins, suggested replacements, applicability, and rendered text. New fields and enum values can appear. Proposed requirement: preserve this structure so ownership/lifetime failures keep their causal explanation; do not reduce everything to a line and message. Apply suggestion confidence distinctly from code-action availability. [rustc JSON diagnostics](https://doc.rust-lang.org/rustc/json.html)

### Type safety, unsafe soundness, and performance are separate claims

Miri detects many forms of undefined behavior, including invalid memory accesses, invalid values, aliasing-model violations, and data races. Its maintainers explicitly state that a successful run cannot prove soundness: it examines particular inputs/executions, has limited platform/FFI support, and cannot explore every concurrent schedule. Proposed requirement: preserve test scope, target, seeds, unsupported operations, and safety invariants alongside results. [Miri maintained documentation](https://github.com/rust-lang/miri)

Engineering inference: semantic navigation and compiler acceptance support understanding and type correctness in the checked environment. Domain design, clean ownership boundaries, migration completeness, cancellation/backpressure behavior, and unsafe invariants require additional reasoning and evidence. Performance claims require a representative workload, baseline, measured resource/latency/throughput, and reproducible comparison; none of the navigation interfaces above supplies that proof.
