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
