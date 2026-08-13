# Repository intelligence policy

Before recursively searching source code or modifying source files:

1. Call `repo.orient` for unfamiliar work.
2. Call `repo.prepare_change` before the first modification.
3. Use returned source slices and semantic context. Call `repo.expand_context` when it is insufficient.
4. Do not use recursive grep/find to discover architecture unless the service reports an incomplete index.
5. After modifications, call `repo.validate_change` with the diff.

The repository intelligence service is authoritative for indexed symbol references, type relationships, Cargo dependencies, decisions, use cases, and Git co-change history.
