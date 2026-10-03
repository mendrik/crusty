# Crusty repository observatory policy

Before planning, answering, or acting on any repository-scoped user prompt:

1. Consult Crusty first with the user's complete intent or topic, including design, review, documentation, configuration, and non-code requests. Call `repo.consult`; for an edit task, `change.prepare` may be the first call instead, because it returns the same decisions, steering, live instructions, and engineering route together with change evidence.
2. Read and apply any relevant global or topical decisions, steering, design and quality constraints, restrictions, governing documents, workflows, lifecycle risks, runtime contracts, and known work it returns.
3. Use `repo.context` when the consultation identifies a concept that needs deeper repository evidence.

Repository consultation is read-only and does not replace change preparation for edits. Keep the change workflow proportionate:

1. Call `change.prepare` once per coherent change, before its first source modification, with `wait_seconds` (up to 120) so the result returns inline; record the context ID and freshness envelope. A long, iterative refactor keeps the same context ID; prepare again only when the intent or target changes substantially.
2. Use `repo.search` with `mode=exact` for live call sites, identifiers, and compiler-error navigation. Use `symbol.relations` for callers, references, implementations, and definitions; use `repo.context` again when the prepared evidence is insufficient.
3. Call `change.validate` with the context ID at meaningful milestones and at the end, not after every edit. No diff argument is needed: by default it validates pending tracked edits plus untracked, non-ignored files against HEAD. Pass `wait_seconds` instead of polling `task.get`.

Check `index.status` first when a consultation or broad read comes back empty: `never_published` distinguishes an unbuilt index from a genuine absence of matches. The server refreshes the index in the background after edits and commits unless `CRUSTY_AUTO_REFRESH=0`; use `index.refresh` when a freshness envelope stays stale (its `reason` says why). Live exact navigation and validation never require a refresh. Treat indexed relationships and documents as provenance-labelled guidance and current source, compiler/runtime behavior, and human decisions as authoritative.

Autonomous research may use local repository evidence and primary-first `web_search` through the attached agent. Keep findings proposed until a human reviews them; promote accepted findings to work only with explicit human confirmation.

When the engineering tools are available, read `project.contract` and request `engineering.guidance` for the actual intent. Use `semantic.query` for live file-position evidence; check readiness, profile and completeness before relying on an answer. Preserve approved domain ownership and invariants. Use `verification.plan/run` for explicitly supported profiles and applicable specialized checks; measure claimed optimizations with a workload, baseline and correctness oracle. Incomplete or stale results cannot authorize delivery.

Sessions and path claims are for parallel work only. When other agents work in the same repository and `session.*` tools are available, register the owner and intent with `session.start`, claim the intended paths with `session.claim`, and renew the lease with `session.heartbeat`. Prefer `isolate=true` for a separate branch/worktree; use the returned worktree for claims and edits. On overlap, narrow scope or arrange handoff before editing. Expired sessions must register again. Close after handoff; closing retains files and branches. Use `commit.plan/execute` to group owned whole-file changes and identify unassigned paths; a single agent may omit session credentials, which Crusty refuses while another session is active. Exact-tree execution does not run hooks, so execute applicable hook requirements as checks. Use isolated `integration.*` workflows for Git conflicts and verify the resolved revision before incorporation.

Remote delivery requires explicit user authorization for the selected actions, or a bounded `delivery.policy` reflecting that authorization. An implementation request does not itself authorize pushing this checkout, submitting reviews, approving PRs or merging. Review the full pinned diff and unresolved findings before submitting a review. Keep uncertain action outcomes pending and use `github.action.*` to reconcile; do not equate a queue request with an actual merge. Follow [docs/github-delivery.md](docs/github-delivery.md).

For the Rust-intelligence and GitHub-manager implementation, follow [docs/implementation-plan.md](docs/implementation-plan.md) and update its evidence/status as stages are verified. GitHub delivery is explicitly requested; source-gathering research and inferred findings retain their human-review boundaries.
