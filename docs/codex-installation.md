# Install and upgrade Crusty in Codex

Crusty 0.3 is a local stdio MCP server. The executable runs on the Codex host, uses the current project directory unless `--workspace` is supplied, and keeps repository intelligence under the repository's ignored `.rust-repo-intelligence/` directory. It does not require an API key or a hosted Crusty service.

Codex's desktop app, CLI, and IDE extension share MCP configuration on the same host. Global configuration lives in `~/.codex/config.toml`; trusted projects may use `.codex/config.toml`. These locations and the stdio server fields are defined by the [official Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

## Install from source

From a Crusty checkout:

```bash
cargo install --path . --locked
```

This installs the `rust-repo-intelligence` executable. Confirm the resolved path before registering it:

```bash
command -v rust-repo-intelligence
```

Rust-analyzer support is opt-in because the companion holds a whole workspace model in memory (often several GB for large workspaces). When enabled, Crusty uses `RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH` if set, otherwise asks `rustup` for the rust-analyzer path, and falls back to `rust-analyzer` on `PATH`; `semantic.status` reports whether the binary was found. The companion starts loading the workspace in the background about a second after server start, never during MCP initialization. Crusty remains usable with syntax-derived, ambiguity-suppressed evidence when rust-analyzer is unavailable. Install it with `rustup component add rust-analyzer`.

## Register the global MCP server

For a new installation:

```bash
CRUSTY_BIN="$(command -v rust-repo-intelligence)"
codex mcp add Crusty -- "$CRUSTY_BIN"
codex mcp get Crusty
```

The equivalent `~/.codex/config.toml` entry is:

```toml
[mcp_servers.Crusty]
command = "/absolute/path/to/rust-repo-intelligence"```

rust-analyzer is off by default; agents call `semantic.enable` when they need live semantics or diagnostics and `semantic.disable` to release it. To start it with every server instead, add `--env RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART=1` (or the same key under `[mcp_servers.Crusty.env]`). The older `RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER` no longer starts it and can be removed.

Use an absolute executable path so graphical clients do not depend on shell-specific `PATH` initialization. Add `args = ["--workspace", "/absolute/path/to/project"]` only for a project-pinned server. A global server should normally omit `args`, allowing Codex to start Crusty in the active project directory.

After adding the server, restart the Codex desktop app or IDE extension. For Codex CLI, begin a new session. Replacing the executable does not hot-swap an MCP process that is already running. Use `codex mcp list`, `codex mcp get Crusty`, or `/mcp` in an interactive client to confirm that the server is enabled.

## Upgrade an existing installation

From the updated checkout:

```bash
cargo install --path . --force --locked
codex mcp get Crusty
```

Keep the existing MCP entry when it already points to the installed executable. Restart the Codex client so it launches Crusty 0.3 instead of retaining the old process.

If `config.toml` contains per-tool approval rules, rename or remove rules that target the 0.1 API. A conservative read/workflow mapping is:

| Crusty 0.1 | Crusty 0.3 |
| --- | --- |
| `repo.locate` | `repo.search` with `mode=exact` or `mode=broad` |
| `repo.orient`, `repo.context_pack` | `repo.context` |
| `repo.prepare_change` | `change.prepare` with `wait_seconds` |
| `repo.validate_change` | `change.validate` with `wait_seconds`; no diff argument needed |
| `repo.status`, `repo.refresh` | `index.status`, `index.refresh` |
| `repo.work.list`, `repo.work.next` | `work.list`, `work.recommend` |
| `repo.work.propose`, `repo.work.update` | `work.create`, `work.update` |

The 0.3 API exposes explicit decision/steering and safe checkpoint operations alongside the bounded problem/quality lifecycle, architecture audits, live semantics, engineering guidance, profile-bound verification and coordinated delivery. Human-owned records and remote actions retain their explicit authorization boundaries. New research discoveries still flow through proposed findings and human review. Consult the [current public API](../README.md#public-mcp-api) and [delivery policy](github-delivery.md).

Project instructions such as `AGENTS.md` must also stop naming removed tools. They should require consultation before acting on every repository-scoped prompt and keep the edit workflow proportionate. A minimal template:

```markdown
# Crusty repository policy

Before planning, answering, or acting on any repository-scoped prompt, consult Crusty with the
user's complete intent: call `repo.consult`, or for an edit task `change.prepare`, which returns the
same decisions, steering, live instructions and engineering route plus change evidence. Apply what
it returns.

For edits, call `change.prepare` once per coherent change (pass `wait_seconds` to get the result
inline) and keep its context ID for the whole change. Call `change.validate` with that context ID at
milestones and at the end; it needs no diff argument and includes new untracked files. Use
`repo.search` with `mode=exact` and `symbol.relations` for live navigation.

Use `session.*` and path claims only when other agents work in the same repository. A single agent
may call `commit.plan`/`commit.execute` without session credentials.

Findings stay proposals until a human reviews them. Remote delivery (push, PR, review, merge)
requires explicit user authorization or a bounded `delivery.policy`.
```

The repository's own [`AGENTS.md`](../AGENTS.md) is a fuller 0.3 policy example.

## Migrate a project

There is no manual database conversion command and no need to edit application source.

On the first 0.2-or-newer open of a repository, Crusty automatically creates `.rust-repo-intelligence/memory.sqlite3`, copies legacy work into the current work store, and preserves legacy decisions, steerings, problem records, and learned quality constraints. The legacy index database is not deleted or rewritten by that import.

Derived repository evidence is different. By default each server runs a background refresher that rebuilds an older store shortly after startup and republishes after edits and commits. With `CRUSTY_AUTO_REFRESH=0`, or to force a rebuild, start one refresh explicitly and poll it to completion:

1. Call `index.status` to inspect the resolved root, the published generation, and `freshness.reason`.
2. Call `index.refresh` with `scope="workspace"` and `wait_seconds=120`.
3. If the refresh outlasts the wait, call `task.get` with the returned task ID and `wait_seconds` until it is `completed` or `failed`.
4. Confirm `index.status` reports `index.snapshot.indexer_version: "9"` and `index.backend.embedding_card_version: "symbol-card-v1"`.

Projects can migrate lazily when next opened. Before the refresh, `repo.search(mode=exact)` remains authoritative for live textual navigation. Broad search and `repo.context` continue serving the previous published generation with an explicit stale label.

## Verify the installation

A successful installation reports server name `Crusty` and the package version. Inspect MCP `tools/list` for the installed capability surface rather than relying on a stale tool count. A useful smoke sequence is:

1. `repo.consult` with the intended repository task
2. `repo.authority`; also confirm the consultation response contains freshness and governing guidance sections
3. `index.status`
4. `repo.search` with `mode=exact`, then `symbol.relations` with `relation=callers`
5. `repo.architecture`; confirm facts and findings remain separately labelled and evidence-bounded
6. `audit.start` with `wait_seconds`, then `audit.get`; confirm the report persists without entering human review automatically
7. `repo.context` after an explicit project refresh
8. `memory.search` with a phrase from a prior project prompt; confirm unrelated repositories and assistant/tool output are absent. Prompts come from Codex history (`$CODEX_HOME`, default `~/.codex`) and Claude Code transcripts (`$CLAUDE_CONFIG_DIR/projects`, default `~/.claude/projects`), each labelled with its `source`
9. `task.list` and `research.list`; confirm bounded summaries can recover interrupted workflows

Crusty research never opens arbitrary external connectors. `research.start` produces a bounded evidence packet; the attached agent performs primary-first `web_search` and returns qualified evidence through `research.submit`. Findings remain proposals until a human reviews them, and promotion to work requires explicit human confirmation.
