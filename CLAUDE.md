# CLAUDE.md

> **Reference config.** This file provides project context to AI coding agents (Claude Code, Codex, etc.). Use it as a starting point — adapt the playbooks, paths, and tool references to match your setup.

## ESS agent onboarding

ESS is the Email Search Service for local-first email indexing/search with:
- CLI (`ess ...`)
- MCP server (`ess mcp`)
- SQLite canonical storage + Tantivy search index

Primary objective when modifying ESS: keep CLI/MCP behavior stable while preserving data correctness between SQLite and Tantivy.

## Fast orientation

- Binary entrypoint: `src/main.rs`
- Data layer: `src/db/`
- Search/index layer: `src/indexer/`, `src/search/`
- Connectors:
  - `src/connectors/json_archive.rs`
  - `src/connectors/graph_api.rs`
  - `src/connectors/gmail_api.rs`
- MCP:
  - `src/mcp/server.rs`
  - `src/mcp/tools.rs`

Runtime storage defaults:
- DB: `~/.ess/ess.db`
- Index: `~/.ess/index`

## Build and test

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

CLI smoke:

```bash
target/release/ess --help
target/release/ess stats --json
target/release/ess search "hello" --limit 5 --json
```

MCP smoke:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | target/release/ess mcp
```

## MCP tools and common calls

Available tools:
- `ess_search`
- `ess_thread`
- `ess_contacts`
- `ess_recent`
- `ess_stats`

Example request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "ess_recent",
    "arguments": {
      "scope": "professional",
      "unread_only": true,
      "limit": 20
    }
  }
}
```

Scope parsing accepts `professional|pro|personal|all`.

## Common task playbooks

### Add or change CLI behavior

1. Update argument structs/enums in `src/main.rs`.
2. Keep both table and `--json` output paths working.
3. Add/adjust tests (or integration checks) for new behavior.

### Change search ranking or filters

1. Update logic in `src/search/filters.rs` or `src/indexer/mod.rs`.
2. Confirm query parsing, date range handling, and scope semantics.
3. Validate UTF-8-safe snippet behavior in `src/search/mod.rs`.
4. Run reindex + search smoke tests.

### Update Graph sync

1. Edit `src/connectors/graph_api.rs`.
2. Preserve token caching and delta-link sync_state keys.
3. Handle rate limits (`429`) and transient retries safely.
4. Ensure delete events remove records from both DB and index.

### Update Gmail sync

1. Edit `src/connectors/gmail_api.rs`.
2. Preserve enumerate-diff-batch pattern and historyId watermarks.
3. Handle rate limits (429) with exponential backoff.
4. Use buffered writes with per-batch commit.

## Data and consistency rules

- SQLite is source-of-truth for email rows.
- Tantivy index must reflect SQLite for searchable fields.
- `reindex` must always be able to rebuild index from DB alone.
- Keep JSON logging/output separation: logs to stderr, machine output to stdout.

## Credentials and security

Graph credentials are resolved from:
- `ESS_TENANT_ID`
- `ESS_CLIENT_ID`
- `ESS_CLIENT_SECRET`
- account config JSON as fallback (for per-account overrides)

Gmail credentials are resolved from:
- `ESS_GMAIL_CLIENT_ID`
- `ESS_GMAIL_CLIENT_SECRET`
- `ESS_GMAIL_REFRESH_TOKEN`
- account config JSON as fallback (for per-account overrides)

Do not print secrets in logs, errors, or tests.

## Code review checklist

- No regressions in `--json` output shape.
- No panics on Unicode text boundaries.
- DB schema/migration changes are backward compatible.
- New commands/subcommands appear in `--help`.
- MCP `tools/list` and `tools/call` still operate via JSON-RPC 2.0.

