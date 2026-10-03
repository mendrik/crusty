# Coordinate parallel coding sessions

Crusty shares coding sessions and ownership claims across all linked Git worktrees. Repository indexes and prepared-change evidence remain local to the observed worktree. Claims coordinate participating agents; editors outside Crusty's workflow are not intercepted.

Sessions exist for parallel work. A single agent working alone needs no session, claims or heartbeats: it consults, prepares, edits, validates and, when it wants Crusty to commit, calls `commit.plan` and `commit.execute` without credentials (see [single-agent commits](#single-agent-commits)). Register a session as soon as another agent may work in the same repository.

## Start and isolate

Call `session.start` with `owner`, `intent`, optional `work_ids`, and `isolate=true` when beginning independent branch work. The response is a task ID; pass `wait_seconds` to receive the settled task inline instead of polling `task.get`. A successful result contains the session and a private `lease_token`. Owner names starting with `crusty:` are reserved. Use its returned `worktree` as the working directory for edits and future Crusty calls.

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

Both calls are task-backed and accept `wait_seconds`. Call `commit.execute` with the credentials and `plan_id`. It rechecks claims, branch, head and file fingerprints; stale plans fail. Execution builds all planned commits through a private Git index, persists their object IDs, and advances HEAD with an expected-old-head comparison. It updates only the selected entries in the real index and retains unrelated staged work. Commit signing follows `commit.gpgsign`; commit hooks are not run. Execute repository hook requirements as explicit checks. `commit.get` recovers the plan/execution; replaying the same plan recovers its recorded outcome rather than creating another chain.

### Single-agent commits

Omit both `session_id` and `lease_token` from `commit.plan` for single-agent work. Inside the same immediate transaction that fences claim acquisition, Crusty checks the shared ledger: if any other coding session is active in any linked worktree, planning is refused and an explicit session is required, exactly as before. Otherwise it registers an ephemeral session owned by `crusty:implicit-commit` that claims exactly the planned changed files, so an agent that registers afterwards sees those paths as owned. An earlier implicit session of the same worktree is closed first, so an abandoned plan never blocks the next one. The plan reports `implicit_session: true`; its lease token is never returned.

Call `commit.execute` with `plan_id` alone. It is authorized by the implicit owner and re-checks, before building objects and again before advancing `HEAD`, that the implicit session is still active (ten-minute lease) and that no other session has started; otherwise it fails without moving `HEAD`, and the work must be claimed under an explicit session and planned again. All other plan checks (branch, `HEAD`, file fingerprints) are unchanged. Successful execution closes the implicit session and releases its claims; replaying the plan returns the recorded outcome. Chunks, integrations and GitHub delivery still require an explicit session. Supplying only one of the two credentials is an error.

### Chunks

After execution, `chunk.create` records the plan's immutable commit chain, title, summary and optional validation references. References alone are not proof that checks passed. `chunk.list/get` recover deliverables. Plan files are whole-file selections; the protocol does not attribute separate hunks within one file.

## Integrate and resolve

Call `integration.preview` with `source_ref` and `base_ref`. Git evaluates pinned revisions without touching either original worktree or index. `clean=false` denotes a Git conflict, independently of ownership overlap.

`integration.start` takes session credentials and `preview_id`, records the intent, and creates a separate integration branch/worktree at the pinned base. Resolve conflicts in its returned worktree and stage the intentional resolutions. `integration.resolve` rejects remaining unmerged paths, unstaged changes, unexpected head/merge state and untracked resolution files. It records the merge commit before advancing HEAD and supports recovery after that advance. Keep renewing the owning session from its registered worktree.

Start Crusty against the integration worktree, run `verification.plan/run`, and pass the current successful run to `integration.complete`. Completion retains the verified branch for [PR delivery](github-delivery.md); it does not overwrite a checked-out main branch. `integration.get` recovers previews and resolutions. Interrupted starts retain the intent and any created worktree for inspection; inspect a `creating` record before starting again. Failed or abandoned worktrees are never deleted automatically.

## Finish

Call `session.close` with the session credentials after completing or handing off the work. Claims are released and the session becomes terminal. Its branch, worktree and files are retained. Closing never discards changes.

Crusty serializes claim acquisition with immediate SQLite transactions and uses a shared Git-mutation lease for managed worktree creation/planning. Raw Git and file edits from other processes still require cooperation. For parallel work, agent instructions must require ownership claims, heartbeat and handoff for the protocol to be effective; the implicit single-agent path is refused as soon as any participating session is active.
