# Rust engineering through Crusty

Crusty supplies repository evidence and engineering judgment to the attached LLM. The LLM still interprets intent, writes code, reviews complete changes and resolves domain conflicts. The compiler, runtime, measurements and human decisions determine whether that work is correct.

## Establish the contract

Start every repository request with `repo.consult`. Current instruction files and accepted domain models accompany snapshot-labelled repository guidance. Current instruction files are bounded; `complete=false` or an omission requires reading the relevant scoped file directly. Call `project.contract` for live workspace members, feature declarations, targets, MSRV metadata, dependencies and instruction/Cargo/CI/configuration evidence. This reports declared capabilities; it does not infer that every feature combination or target is supported.

Request `engineering.guidance` with the intent and optional areas: `core`, `ownership`, `api`, `architecture`, `cleanup`, `concurrency`, `unsafe`, `performance`, `verification`, `cargo`, `specialized`. Versioned rules include rationale, exceptions and required evidence. They cover canonical typed boundary data, private invariant-preserving constructors, cohesive owners, closed consequential states, dependency direction, explicit resource/task lifecycles, complete migrations, errors and measurements. Advice remains conditional; it does not activate a new blocking policy.

`domain.model.propose` records concept owners, invariants, mutation rights and allowed/forbidden module dependencies. `domain.model.review` accepts or rejects only at an explicit human review, with attribution and reason. Accepted models appear in consultation and preparation. Supersession is atomic and retained as history. This is an explicit design contract, not an automated proof that source implements every invariant or dependency restriction.

## Query live Rust semantics

Enable `RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER=1` and provide an installed rust-analyzer. `semantic.status` reports its configuration without implicitly starting it. `semantic.query` takes a repository-relative `file`, one-based `line`, zero-based UTF-16 `character`, query and optional Cargo profile. It supports hover, definition/type definition, references, implementations, incoming/outgoing calls, macro expansion, rename, assists, signature help and dependency list. Rename additionally requires `new_name`.

Queries synchronize current source and configure actual Cargo features, target and toolchain. The companion is reused while its evidence/profile matches and restarted when those inputs change or it exits. Results distinguish disabled, unsupported, failed, stale, loading/unconfirmed, empty and ready states. `complete` requires source stability, a healthy quiescent server and untruncated results. Empty is not proof that no runtime/generated/external consumer exists. Call-hierarchy expansion is bounded. External locations and dependency information come from rust-analyzer; missing sources or unsupported extensions remain explicit.

Rename/assist workspace edits are proposals. Crusty does not apply them or execute commands supplied by the LSP. Claim their complete affected surface, prepare, verify document versions/fingerprints, apply the intended edits and validate. Position queries work before index publication; broad retrieval remains snapshot-based discovery.

## Verify supported configurations

Use `verification.plan` with explicit `profile`: member `packages`, `features`, `no_default_features`, `all_features`, optional `target` and installed `toolchain`. Empty packages mean the whole workspace. Run separate plans for declared supported combinations; do not blindly combine mutually exclusive features. Choose installed MSRV/target profiles from the project contract and requirements. Missing tools or dependencies produce failure, not invented coverage.

Default checks are format, all-target check, workspace tests including doctests, and all-target Clippy. Additional kinds include doctests, documentation, release, benches and Miri. Plans always use `--locked`; `offline=true` keeps dependency access offline. Warning denial follows current lint evidence unless explicitly set with `deny_warnings`. `test_filter` supports focused regressions but focused results cannot authorize delivery. Specialized fuzz, Loom, ABI/API/dependency or profiler evidence still requires the relevant external tools and explicit task-specific execution; Crusty does not synthesize those results.

`verification.run` is a durable task. Poll `task.get`, then recover the full plan/run through `verification.get` if necessary. Reports retain commands, status, timings, stdout/stderr artifacts and bounded complete compiler messages, including child explanations, secondary spans and suggestions. Omitted diagnostics are labelled. A failed check stops the requested sequence; later unexecuted checks are not passing evidence.

Plans/runs bind to head, workspace content including untracked inputs, executable bits, relevant environment digest, tool versions and profile. Editing any input invalidates a pending plan or delivery eligibility. A successful full default plan on the current clean revision is required for managed delivery; additional required matrices/specialized checks remain the agent's explicit project obligations. `change.validate` uses this same runner when checks are requested and also reports prepared architecture and quality obligations. Task completion means a report exists; inspect its verdict.

## Clean complete ownership slices

`cleanup.plan` takes a proposed canonical owner, obsolete identifiers, rationale and optional scope. It inventories current source, tests, Cargo manifests, configuration, scripts and docs, adding provenance-labelled indexed relationships. It reports skipped inputs and occurrence limits and binds the inventory to a source digest. `cleanup.get` reports staleness. Textual matches do not establish deadness, and no matches do not establish absence of external/generated/runtime contracts. The attached agent completes the migration, removes replaced paths, verifies legitimate consumers and repeats the sweep.

## Measure optimization claims

Record `performance.contract` before changing performance: workload, positive `wall_time_budget_ms`, operations per invocation and rationale. `performance.measure` identifies baseline/candidate Git revisions, declared Cargo package/binary, profile, arguments, warmup and samples. It builds release binaries in isolated retained worktrees and records tool/build/profile information, timing samples, output artifacts and distributions. The original worktree is untouched.

Default `metric_source=stdout_json` requires the workload binary to emit a JSON object containing positive `elapsed_ns` and stable result/checksum fields. Crusty compares those result fields across both revisions and samples. `process_wall_time` explicitly includes process startup and exit polling overhead of up to 25 ms, unsuitable for resolving tiny changes. `allow_output_difference=true` requires separate behavioral evidence. Measurements describe the observed workload and environment; they do not prove statistical significance, allocation behavior, race freedom or universally optimal code. Custom external Cargo target directories are currently unsupported by this workload adapter. `performance.get` preserves partial failures, artifacts and retained worktrees.

The [evaluation corpus](engineering-evaluation.md) tests independent behavioral contracts. The [gap analysis](rust-engineering-gap-analysis.md) explains why live semantics, executable checks and measured outcomes matter more than an ever-longer generic instruction prompt.
