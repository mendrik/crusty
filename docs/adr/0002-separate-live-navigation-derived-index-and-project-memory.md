# Separate live navigation, the derived index, and project memory

Crusty reads exact Rust navigation from the live worktree, serves broader intelligence from an atomically published derived-index generation, and stores findings, research, tasks, and human-owned work in a separate durable memory database. Refresh is explicit and asynchronous, and a filesystem publisher lease permits only one process to write a generation. This keeps stale or slow indexing off the interactive path and prevents evaluation or secondary server processes from racing the active publisher.

## Consequences

Every indexed answer carries a freshness envelope. The index is disposable, project memory is preserved during rebuilds, and validation may use stale labelled evidence but never waits for refresh.

## Amendment: background refresh

Explicit-only refresh left long-running servers answering from weeks-old generations. Each server process now owns one background refresher that runs the same incremental refresh as a durable task under the publisher lease after debounced file changes and `HEAD` moves (opt out with `CRUSTY_AUTO_REFRESH=0`). Refresh stays asynchronous and off the interactive path: reads still serve the last published generation and never wait.
