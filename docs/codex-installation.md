# Install and upgrade Crusty in Codex

Crusty 0.2 is a local stdio MCP server. The executable runs on the Codex host, uses the current project directory unless `--workspace` is supplied, and keeps repository intelligence under the repository's ignored `.rust-repo-intelligence/` directory. It does not require an API key or a hosted Crusty service.

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

Rust-analyzer support is opt-in. When enabled, Crusty first asks `rustup` for the rust-analyzer path and falls back to `rust-analyzer` on `PATH`. Crusty remains usable with syntax-derived, ambiguity-suppressed evidence when rust-analyzer is unavailable.

## Register the global MCP server

For a new installation:

```bash
CRUSTY_BIN="$(command -v rust-repo-intelligence)"
codex mcp add Crusty \
  --env RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER=1 \
  -- "$CRUSTY_BIN"
codex mcp get Crusty
```

The equivalent `~/.codex/config.toml` entry is:

```toml
[mcp_servers.Crusty]
command = "/absolute/path/to/rust-repo-intelligence"

[mcp_servers.Crusty.env]
RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER = "1"
```

Use an absolute executable path so graphical clients do not depend on shell-specific `PATH` initialization. Add `args = ["--workspace", "/absolute/path/to/project"]` only for a project-pinned server. A global server should normally omit `args`, allowing Codex to start Crusty in the active project directory.

After adding the server, restart the Codex desktop app or IDE extension. For Codex CLI, begin a new session. Replacing the executable does not hot-swap an MCP process that is already running. Use `codex mcp list`, `codex mcp get Crusty`, or `/mcp` in an interactive client to confirm that the server is enabled.

## Upgrade an existing installation

From the updated checkout:

```bash
cargo install --path . --force --locked
codex mcp get Crusty
```

Keep the existing MCP entry when it already points to the installed executable. Restart the Codex client so it launches Crusty 0.2 instead of retaining the old process.

If `config.toml` contains per-tool approval rules, rename or remove rules that target the 0.1 API. A conservative read/workflow mapping is:

| Crusty 0.1 | Crusty 0.2 |
| --- | --- |
| `repo.locate` | `repo.search` with `mode=exact` or `mode=broad` |
| `repo.orient`, `repo.context_pack` | `repo.context` |
| `repo.prepare_change` | `change.prepare`, then poll `task.get` |
| `repo.validate_change` | `change.validate`, then poll `task.get` |
| `repo.status`, `repo.refresh` | `index.status`, `index.refresh` |
| `repo.work.list`, `repo.work.next` | `work.list`, `work.recommend` |
| `repo.work.propose`, `repo.work.update` | `work.create`, `work.update` |

The 0.2 API intentionally removes direct public administration of legacy decisions, checkpoints, problems, and quality constraints. Existing quality memory can still contribute evidence to prepared changes and validation, while new improvement discovery flows through research, proposed findings, human review, and explicitly human-owned work.

Project instructions such as `AGENTS.md` must also stop naming removed tools. The repository's own [`AGENTS.md`](../AGENTS.md) is a minimal 0.2 policy example.

## Migrate a project

There is no manual database conversion command and no need to edit application source.

On the first 0.2 open of a repository, Crusty automatically creates `.rust-repo-intelligence/memory.sqlite3`, copies legacy work into the single 0.2 work store, and preserves legacy decisions, steerings, problem records, and learned quality constraints. The legacy index database is not deleted or rewritten by that import.

Derived repository evidence is different: 0.2 does not perform implicit refreshes. In each existing project, explicitly start one refresh and poll it to completion:

1. Call `index.status` to inspect the published generation and staleness.
2. Call `index.refresh` with `scope="workspace"`.
3. Poll the returned task ID with `task.get` until it is `completed` or `failed`.
4. Confirm the refresh result reports `indexer_version: "8"` and `card_version: "symbol-card-v1"`.

Projects can migrate lazily when next opened. Before the refresh, `repo.search(mode=exact)` remains authoritative for live textual navigation. Broad search and `repo.context` continue serving the previous published generation with an explicit stale label.

## Verify the installation

A successful installation reports server name `Crusty`, version `0.2.1`, and the clean-break tool surface. A useful smoke sequence is:

1. `repo.authority`
2. `index.status`
3. `repo.search` with `mode=exact`
4. `repo.context` after an explicit project refresh
5. `memory.search` with a phrase from a prior project prompt; confirm unrelated repositories and assistant/tool output are absent

Crusty research never opens arbitrary external connectors. `research.start` produces a bounded evidence packet; the attached agent performs primary-first `web_search` and returns qualified evidence through `research.submit`. Findings remain proposals until a human reviews them, and promotion to work requires explicit human confirmation.
