# Coordinate parallel coding sessions

Crusty shares coding sessions and ownership claims across all linked Git worktrees. Repository indexes and prepared-change evidence remain local to the observed worktree. Claims coordinate participating agents; editors outside Crusty's workflow are not intercepted.

## Start and isolate

Call `session.start` with `owner`, `intent`, optional `work_ids`, and `isolate=true` when beginning independent branch work. The response is a task ID. Poll `task.get` until settled; a successful result contains the session and a private `lease_token`. Use its returned `worktree` as the working directory for edits and future Crusty calls.

Isolation creates a fresh branch/worktree at the caller's committed HEAD. It preserves dirty files in the original worktree and does not copy them. A registered non-isolated session (`isolate=false`, the default) uses the current worktree. Isolation requires an existing Git commit; non-Git directories support registration and claims without isolation.

The coordinator database is under the canonical Git common directory, so sessions in different linked worktrees observe the same owners. It is independent of the rebuildable source index. Registrations reference work IDs but do not create or promote project work.

## Claim and renew

Call `session.claim` from the registered worktree with `session_id`, `lease_token`, and relative file/subtree `paths`. The claim set replaces the session's previous set atomically. `.` covers the whole repository; an empty array releases claims. Absolute paths, parent traversal, repository-internal state, and symlink aliases are rejected.

Overlapping active claims return `acquired=false` plus the other owner's intent, worktree, path and expiry. A failed acquisition preserves all earlier claims. Choose disjoint scope or explicitly hand work over by releasing/closing the previous claim before acquiring it. A claim overlap is not a Git merge result.

The default lease is ten minutes. `ttl_seconds` accepts 30 through 3600 seconds. Renew before expiry with `session.heartbeat`; an optional `summary` explains current activity to others. Expiry is terminal. A resumed agent registers a new session and reacquires ownership; old capability tokens cannot revive the previous lease.

`session.list` returns active owners by default, plus pagination metadata. `include_inactive=true` includes closed and expired records. `session.get` inspects one record. Neither exposes lease tokens or token hashes.

## Plan coherent commits

Call `commit.plan` with the session credentials and `groups`, each containing a descriptive `message` and a set of files/subtrees. This is task-backed. The result expands groups against live Git status, verifies ownership of every selected changed file, rejects duplicate assignment, and reports unassigned changed paths. Rename source and destination are both part of the change surface.

Plans persist the exact head, branch, groups, and content fingerprints. Planning neither stages nor commits; other agents' staged files stay untouched. Whole-file claims do not divide ownership of separate hunks inside one file. Git unmerged paths must be resolved before planning.

Call `commit.execute` with the credentials and `plan_id`. It rechecks claims, branch, head and file fingerprints; stale plans fail. Execution builds all planned commits through a private Git index, persists their object IDs, and advances HEAD with an expected-old-head comparison. It updates only the selected entries in the real index and retains unrelated staged work. Commit signing follows `commit.gpgsign`; commit hooks are not run. Execute repository hook requirements as explicit checks. `commit.get` recovers the plan/execution; replaying the same plan recovers its recorded outcome rather than creating another chain.

After execution, `chunk.create` records the plan's immutable commit chain, title, summary and optional validation references. References alone are not proof that checks passed. `chunk.list/get` recover deliverables. Plan files are whole-file selections; the protocol does not attribute separate hunks within one file.

## Integrate and resolve

Call `integration.preview` with `source_ref` and `base_ref`. Git evaluates pinned revisions without touching either original worktree or index. `clean=false` denotes a Git conflict, independently of ownership overlap.

`integration.start` takes session credentials and `preview_id`, records the intent, and creates a separate integration branch/worktree at the pinned base. Resolve conflicts in its returned worktree and stage the intentional resolutions. `integration.resolve` rejects remaining unmerged paths, unstaged changes, unexpected head/merge state and untracked resolution files. It records the merge commit before advancing HEAD and supports recovery after that advance. Keep renewing the owning session from its registered worktree.

Start Crusty against the integration worktree, run `verification.plan/run`, and pass the current successful run to `integration.complete`. Completion retains the verified branch for [PR delivery](github-delivery.md); it does not overwrite a checked-out main branch. `integration.get` recovers previews and resolutions. Interrupted starts retain the intent and any created worktree for inspection; inspect a `creating` record before starting again. Failed or abandoned worktrees are never deleted automatically.

## Finish

Call `session.close` with the session credentials after completing or handing off the work. Claims are released and the session becomes terminal. Its branch, worktree and files are retained. Closing never discards changes.

Crusty serializes claim acquisition with immediate SQLite transactions and uses a shared Git-mutation lease for managed worktree creation/planning. Raw Git and file edits from other processes still require cooperation. Agent instructions must require ownership claims, heartbeat and handoff for the protocol to be effective.
