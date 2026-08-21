# Separate live navigation, the derived index, and project memory

Crusty reads exact Rust navigation from the live worktree, serves broader intelligence from an atomically published derived-index generation, and stores findings, research, tasks, and human-owned work in a separate durable memory database. Refresh is explicit and asynchronous, and a filesystem publisher lease permits only one process to write a generation. This keeps stale or slow indexing off the interactive path and prevents evaluation or secondary server processes from racing the active publisher.

## Consequences

Every indexed answer carries a freshness envelope. The index is disposable, project memory is preserved during rebuilds, and validation may use stale labelled evidence but never waits for refresh.
