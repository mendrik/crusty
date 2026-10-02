# Crusty engineering intelligence and coordinated delivery

Requested by the repository owner on 2026-10-01. This plan covers the complete [Rust intelligence audit](rust-engineering-gap-analysis.md) and the requested GitHub manager, parallel-session coordination, conflict resolution, coherent commits, and delivery of larger work chunks to main. It supersedes the earlier product restriction against GitHub delivery integration. Autonomous research findings remain proposals; the user's implementation request authorizes this product work.

## Completion contract

An attached LLM can discover current project instructions and Rust semantics, reason from project-specific engineering guidance, obtain and execute the supported verification plan, coordinate work with other sessions, produce coherent commits, submit a work chunk as a PR, review an exact PR revision, and approve/merge through repository policy. Shared state survives process restarts and is shared by linked Git worktrees. Conflicts are detected from both ownership claims and Git integration evidence; resolutions are validated before incorporation. No stale review or check can authorize merging different code.

The compiler, tests, measurements, Git, and GitHub remain authoritative. Crusty provides their intelligence and explicit workflow; it does not claim universally optimal code or turn inferred architecture debt into blocking policy.

## Implementation stages and evidence

| Stage | Deliverables | Acceptance evidence | Status |
| --- | --- | --- | --- |
| 1. Parallel coordination | Shared session ledger, leases/capabilities, atomic subtree claims, overlap reports, isolated worktrees and commit grouping | Racing claims have one winner; expiry/token privacy/path rejection; independent MCP processes share linked-worktree claims and handoff | Implemented and locally tested |
| 2. Git integration and conflicts | Immutable work chunks, exact-tree commits preserving unrelated staging, pinned Git previews, isolated staged resolution and current verification gate | Exact commit contents/replay/stale rejection; conflict preview preserves originals; real two-parent resolution/recovery and stale-check rejection | Implemented and locally tested |
| 3. GitHub manager | Installed gh adapter, explicit repo/actor, bounded PR evidence, verified draft publication, ready/review/approval/merge policy and durable action reconciliation | Instrumented CLI plus real local bare Git push; remote mismatch, self-approval, uncertain review at exhausted budget, stale head and actual merge reconciliation | Implemented; controlled live smoke remains |
| 4. Live Rust intelligence | Opt-in reusable live position queries, profile configuration, hover/definitions/references/implementations, hierarchy/macros/dependencies, proposed rename/assists and explicit health/completeness | Installed rust-analyzer answers before publication and after source edits, Unicode path/positions, macro expansion and unapplied rename; compiler/source authoritative | Implemented; wider extension/profile corpus remains |
| 5. Verification and diagnostics | Current Cargo/instruction/CI contract, explicit member/feature/target/toolchain plans, complete default doctests, focused/specialized checks and bound artifacts | Real virtual workspace and mutually exclusive feature profiles; false doctest fails; compiler children/suggestions retained; source/environment/version binding | Implemented; cross-target/MSRV installations are project-specific |
| 6. Engineering guidance and architecture | Conditional versioned Rust/cleanup/architecture rules, human-reviewed ownership/invariant/dependency models and live migration inventory | Guidance routing/exceptions, model activation/supersession, live multi-surface cleanup fixture; proposed guidance does not activate policy | Implemented; automatic model conformance is future work |
| 7. Measured engineering | Workload contracts, pinned isolated release runs, instrumented/stdout correctness adapter, raw samples/distributions/artifacts; explicit Cargo Miri/bench commands | Real distinct baseline/candidate revisions with stable checksum and retained samples; process-timing limitations and external evidence gaps explicit | Workload adapter implemented; profiler/other specialized adapters remain |
| 8. Evaluation and release | Three fixture repositories, independent behavioral/compile-fail/profile oracles, host-declared conditions/costs, MCP lifecycle tests and user/recovery docs | Broken baselines fail/reference implementations pass; full Cargo suite, real LSP/Git/verification, mock GitHub, latency and retrieval evaluation | Harness implemented; actual comparative LLM trials remain |

These statuses describe the initial implementation and observed local evidence. They do not claim a live GitHub rollout, general LLM superiority, inferred configuration coverage or completed future adapters. Cross-project and actual agent-outcome coverage must grow before retiring the external skills.

## Initial implementation evidence (2026-10-02)

- `cargo test --locked`: 150 library tests, three MCP schema/contract tests and one end-to-end MCP regression test passed; zero crate doctests are declared. The suite includes actual Cargo, installed rust-analyzer, Git conflict resolution and instrumented GitHub recovery.
- `cargo clippy --all-targets --locked -- -D warnings` passed. Formatting and all-target compilation are part of the final checks.
- `python3 tests/engineering_evaluation.py self-test`: all three broken baselines fail and all three reference solutions pass the independent oracles. No LLM was invoked by this self-test.
- Release observatory evaluation: live exact p95 7.12 ms against 150 ms budget, warm intelligence p95 231.81 ms against 750 ms budget on this checkout/machine.
- Release retrieval evaluation: recall@10 0.8 over five queries; one missed `search_nodes`. This remains measurable retrieval debt rather than a perfect-intelligence claim.
- GitHub effects were exercised through a CLI fixture and local bare Git repository. No real PR/review/approval/merge was submitted.

## Remaining rollout and intelligence work

1. Run a controlled live GitHub smoke under an explicit repository/action policy, then validate relevant server/CLI versions, protections and queue behavior. Fork publication and a host-independent background queue reconciler are separate future workflows.
2. Run repeated actual LLM trials for bare/skills/Crusty/combined conditions with fixed models/budgets, unseen cases, behavioral outcomes and measured cost. Expand unsafe/concurrency/large-migration and allocation/performance cases before claiming the skills are replaceable.
3. Add installed MSRV/cross-target and wider rust-analyzer extension fixtures for each supported project. Explicit profile selection is implemented; automatic inference of a complete supported matrix is not.
4. Add profiler/allocation, Loom/fuzz, ABI/API/dependency-specific adapters where project evidence calls for them. Current workload measurements and Cargo checks do not establish those properties.
5. Compare approved model dependency/ownership contracts against semantic source facts, improve dependency-source provenance and affected-profile routing, and tune the measured retrieval miss. Inferred findings remain advisory until a human activates policy.
6. Continue narrowing opaque persisted evidence at domain boundaries as records evolve; typed requests, closed choices, ownership validation and fail-closed revision gates are implemented, while some extensible delivery/evidence envelopes remain JSON.

## Ownership

- The existing observatory owns consultation, public tool routing, durable background tasks, preparation and validation.
- Session coordination owns who is working where and their leased change surfaces. It is shared through Git's common repository identity, independent of worktree-local derived indexes.
- Delivery owns immutable work-chunk/commit/review identities and integration state. Git owns commit content and merge outcomes; GitHub owns remote protections and PR state.
- The semantic service owns LSP configuration, synchronization, query lifecycle and compiler-backed evidence. Broad retrieval remains snapshot-based discovery.
- Verification owns the effective project contract, check planning, execution and revision-bound evidence. Guidance owns conditional engineering expertise; approved domain policy remains human-governed.

Use the existing task runner, execution control, stores and review model where their ownership applies. Add modules for distinct concepts instead of expanding the service into a generic manager. Normalize identifiers, paths and boundary requests once; use enums for state and closed policy choices.

## Delivery authority

Publishing, review submission and merge are explicit user-authorized operations or run under an explicit bounded delivery policy for the selected repository/base/work chunk. Policies may authorize larger batches without repeated permission requests; they cannot bypass remote protections or fabricate human review. A review names the exact head commit, evidence and unresolved findings. GitHub may refuse approval by the PR author. Head changes invalidate review/validation eligibility. Merge queues and asynchronous acceptance require reconciliation until actual merge is observed.

PR descriptions describe the final behavior, substantive design and commands/results. Commit groups correspond to cohesive owned changes. Parallel edits in one worktree require serialized Git mutations; isolated worktrees are the default recommendation. Claim conflicts are coordination evidence, not proof that two branches cannot merge. Git conflict results require domain-aware resolution and new validation.

## Agent contract

Consult first, then identify current governing instructions, acceptance criteria, profile and canonical owners. Register a session and claim the intended change surface before concurrent work. Heartbeat while active; re-plan after a conflict or stale result. Retrieve conditional Rust guidance and live semantics. Prepare before edits, make the complete authorized change, and validate the actual resulting revision. Group commits and publish a verified work chunk. Review and merge only the identified remote head under the applicable delivery policy. Close the session after handoff; do not abandon ownership silently.

## Current workflow record

Initial implementation prepared with `ctx_ff9cd3b1c8d2` (`task_1450ecf1aa60f245`). Published generation 48 at `a4c2e4c05ca68ca65f7bfe3336057b725e8b042d` is stale relative to live `21af7a48a988d004c1fcf5ba4e9829e8045fd075`. Live source, Git state and verification govern implementation. The earlier untracked audit document is preserved.

Full-patch validation includes all new files and uses the same prepared context. Checks were executed directly rather than duplicated by the attached older server, so its validation report correctly says `not_run`. The advisory architecture delta reports no new findings and one worsened existing opaque-error-boundary finding in `Observatory`. This transport-facing command layer retains the repository's contextual `anyhow::Result` interface; stable domain-facing error categories remain an explicit follow-up rather than silently claiming the debt was removed. No blocking quality obligation matched. Local release evaluation republished the derived snapshot as generation 49; subsequent source changes remain freshness-labelled against that generation.

## Primary delivery contracts

The research skill checked these official sources on 2026-10-01. Installed client capabilities must be checked during integration; these references describe upstream contracts, not completed Crusty functionality.

- PR creation must identify base repository/branch and head repository/branch. Explicit `--head` avoids implicit pushing/forking; `--dry-run` can still push, so it is not a safe preview mechanism. [GitHub CLI PR creation](https://cli.github.com/manual/gh_pr_create)
- Bind review packets to head and base OIDs. REST review creation accepts `commit_id`; this pins the reviewed commit but is not an atomic latest-head precondition. Recheck head before and after submission and reconcile ambiguous responses. PR authors cannot approve their own PRs. [GitHub reviews API](https://docs.github.com/en/rest/pulls/reviews#create-a-review-for-a-pull-request), [required-review approval](https://docs.github.com/en/enterprise-cloud%40latest/pull-requests/how-tos/review-pull-requests/approving-a-pull-request-with-required-reviews)
- A merge request must pin the head, preserve repository protections, and distinguish requested, enqueued and actually merged states. Never use an administrative bypass as an ordinary fallback. [GitHub CLI merge](https://cli.github.com/manual/gh_pr_merge), [asynchronous merge API](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request-asynchronously)
- Verify the active authenticated actor rather than treating zero exit from JSON auth-status output as proof. Never expose credentials in task logs. [GitHub CLI authentication](https://cli.github.com/manual/gh_auth_status)
- Linked worktrees have separate indexes/HEADs but shared refs. Canonical common-directory identity is the coordination key. [Git worktrees](https://git-scm.com/docs/git-worktree), [Git repository paths](https://git-scm.com/docs/git-rev-parse)
- Whole-file commit selection is distinct from hunk ownership. Preserve unrelated staged work and use exact path boundaries. [Git commit](https://git-scm.com/docs/git-commit)
- `merge-tree --write-tree` evaluates merge semantics without changing the worktree/index. Exit 0 means clean, 1 means conflicted, and other results are errors; an empty conflicted-path list does not establish a clean merge. [Git merge-tree](https://git-scm.com/docs/git-merge-tree)
