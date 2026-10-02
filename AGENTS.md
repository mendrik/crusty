# Crusty repository observatory policy

Before planning, answering, or acting on any repository-scoped user prompt:

1. Call `repo.consult` first with the user's complete intent or topic, including design, review, documentation, configuration, and non-code requests.
2. Read and apply any relevant global or topical decisions, steering, design and quality constraints, restrictions, governing documents, workflows, lifecycle risks, runtime contracts, and known work it returns.
3. Use `repo.context` when the consultation identifies a concept that needs deeper repository evidence.

Repository consultation is read-only and does not replace the change workflow below.

Before modifying Rust source:

1. Use `repo.context` to map unfamiliar work against the published snapshot.
2. Call `change.prepare` before the first source modification, poll its task with `task.get`, and record the resulting context ID and freshness envelope.
3. Use `repo.search` with `mode=exact` for live call sites, identifiers, and compiler-error navigation. Use `symbol.relations` for callers, references, implementations, and definitions; use `repo.context` again when the prepared evidence is insufficient.
4. After modifications, call `change.validate` with the context ID and diff, then poll its task to completion.

Check `index.status` first when a consultation or broad read comes back empty: `never_published` distinguishes an unbuilt index from a genuine absence of matches. Use `index.refresh` only as an explicit background task; live exact navigation and validation never require it. Treat indexed relationships and documents as provenance-labelled guidance and current source, compiler/runtime behavior, and human decisions as authoritative.

Autonomous research may use local repository evidence and primary-first `web_search` through the attached agent. Keep findings proposed until a human reviews them; promote accepted findings to work only with explicit human confirmation.

When the engineering tools are available, read `project.contract` and request `engineering.guidance` for the actual intent. Use `semantic.query` for live file-position evidence; check readiness, profile and completeness before relying on an answer. Preserve approved domain ownership and invariants. Use `verification.plan/run` for explicitly supported profiles and applicable specialized checks; measure claimed optimizations with a workload, baseline and correctness oracle. Incomplete or stale results cannot authorize delivery.

For parallel work when `session.*` tools are available, register the owner and intent with `session.start`, claim the intended paths with `session.claim`, and renew the lease with `session.heartbeat`. Prefer `isolate=true` for a separate branch/worktree; use the returned worktree for claims and edits. On overlap, narrow scope or arrange handoff before editing. Expired sessions must register again. Close after handoff; closing retains files and branches. Use `commit.plan/execute` to group owned whole-file changes and identify unassigned paths. Exact-tree execution does not run hooks, so execute applicable hook requirements as checks. Use isolated `integration.*` workflows for Git conflicts and verify the resolved revision before incorporation.

Remote delivery requires explicit user authorization for the selected actions, or a bounded `delivery.policy` reflecting that authorization. An implementation request does not itself authorize pushing this checkout, submitting reviews, approving PRs or merging. Review the full pinned diff and unresolved findings before submitting a review. Keep uncertain action outcomes pending and use `github.action.*` to reconcile; do not equate a queue request with an actual merge. Follow [docs/github-delivery.md](docs/github-delivery.md).

For the Rust-intelligence and GitHub-manager implementation, follow [docs/implementation-plan.md](docs/implementation-plan.md) and update its evidence/status as stages are verified. GitHub delivery is explicitly requested; source-gathering research and inferred findings retain their human-review boundaries.
