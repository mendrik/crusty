# Crusty repository observatory policy

Before modifying Rust source:

1. Use `repo.context` to map unfamiliar work against the published snapshot.
2. Call `change.prepare` before the first source modification, poll its task with `task.get`, and record the resulting context ID and freshness envelope.
3. Use `repo.search` with `mode=exact` for live call sites, identifiers, and compiler-error navigation. Use `repo.context` again when the prepared evidence is insufficient.
4. After modifications, call `change.validate` with the context ID and diff, then poll its task to completion.

Use `index.refresh` only as an explicit background task; live exact navigation and validation never require it. Treat indexed relationships as provenance-labelled guidance and current source, compiler/runtime behavior, and human decisions as authoritative.

Autonomous research may use local repository evidence and primary-first `web_search` through the attached agent. Keep findings proposed until a human reviews them; promote accepted findings to work only with explicit human confirmation.
