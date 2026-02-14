# ESS (Email Search Service)

![Rust](https://img.shields.io/badge/rust-2021-orange)
![License](https://img.shields.io/badge/license-MIT-blue)
![Storage](https://img.shields.io/badge/storage-SQLite%20%2B%20Tantivy-4c8)

ESS is a local-first email search service with a CLI and an MCP server. It stores canonical email data in SQLite and indexes searchable content in Tantivy.

## What ESS does

- Imports JSON email archives into a local SQLite database.
- Syncs Microsoft Graph mailbox data (delta sync with token caching).
- Indexes email text for fast full-text search.
- Exposes both CLI commands and MCP tools (`ess_search`, `ess_thread`, `ess_contacts`, `ess_recent`, `ess_stats`).
- Supports multi-account setups with account-type scoping (`professional`, `personal`).

## Prerequisites

- Rust toolchain (`cargo`, Rust 1.75+ recommended)
- Linux/macOS shell
- Optional for Graph sync:
  - Microsoft Graph app credentials (`ESS_CLIENT_ID`, `ESS_CLIENT_SECRET`, `ESS_TENANT_ID`)

## Installation

### Option 1: install from source with cargo

```bash
cd /path/to/ess
cargo install --path .
```

### Option 2: use installer script

```bash
cd /path/to/ess
./scripts/install.sh
```

The installer:
- Builds release binary
- Installs `ess` to `~/.local/bin/ess`
- Creates `~/.ess/config.toml` if missing

### Verify

```bash
ess --version
ess --help
```

## Quick start

### 1. Add an account

```bash
ess accounts add you@company.com professional --tenant-id <tenant-id>
```

`account_id` defaults to the lowercased email address.

### 2. Import a JSON archive

```bash
ess import /path/to/archive --account you@company.com
```

If only one account exists, `--account` is optional.

### 3. Search emails

```bash
ess search "quarterly planning" --from alice@company.com --since 2026-01-01 --limit 20
```

### 4. Inspect results and threads

```bash
ess show <message-id>
ess thread <conversation-id>
```

## CLI reference

Global flags (available on all commands):
- `--json` output JSON instead of table/text
- `--scope <pro|personal|all>` filter by account type

### `ess search <query>`

Search indexed emails.

Example:
```bash
ess search "budget review" --account you@company.com --folder inbox --limit 25
```

Options:
- `--from <email>`
- `--since <YYYY-MM-DD>`
- `--until <YYYY-MM-DD>`
- `--account <account-id>`
- `--folder <folder>`
- `--limit <n>`

### `ess list`

List emails with lightweight filters.

Example:
```bash
ess list --unread --account you@company.com --limit 50
```

Options:
- `--from <email>`
- `--unread`
- `--account <account-id>`
- `--limit <n>`

### `ess show <id>`

Show one email by ID.

Example:
```bash
ess show AAMkAG...
```

### `ess thread <conversation-id>`

Show all messages in a conversation.

Example:
```bash
ess thread AAQkAG...
```

### `ess sync`

Sync configured accounts from Microsoft Graph.

Examples:
```bash
ess sync
ess sync --account you@company.com
ess sync --watch
```

Options:
- `--account <account-id>`
- `--full`
- `--watch`

### `ess import <path>`

Import local JSON archive files.

Example:
```bash
ess import ./fixtures/archive --account you@company.com
```

Options:
- `--account <account-id>`

### `ess contacts`

List/search contacts inferred from emails.

Example:
```bash
ess contacts --query "alice"
```

Options:
- `--query <text>`
- `--enrich` (placeholder; currently prints a notice and returns current data)

### `ess accounts`

Manage account metadata/state.

Examples:
```bash
ess accounts list
ess accounts add you@gmail.com personal
ess accounts remove you@gmail.com
ess accounts sync-status
```

Subcommands:
- `list`
- `add <email> <professional|personal> [--tenant-id <tenant-id>]`
- `remove <account-id>`
- `sync-status`

### `ess stats`

Show DB and index stats.

Example:
```bash
ess stats
```

### `ess reindex`

Rebuild Tantivy index from SQLite source-of-truth.

Example:
```bash
ess reindex
```

### `ess mcp`

Run the MCP server over stdio.

Example:
```bash
ess mcp
```

## MCP setup

Add ESS to your MCP client config:

```json
{
  "servers": {
    "ess": {
      "command": "ess",
      "args": ["mcp"]
    }
  }
}
```

### MCP tool catalog

- `ess_search`: full-text search with filters
- `ess_thread`: fetch messages in a conversation
- `ess_contacts`: search contacts by name/email
- `ess_recent`: list recent emails with optional unread/scope filters
- `ess_stats`: database/index summary

Example `tools/call` payload:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "ess_search",
    "arguments": {
      "query": "security review",
      "scope": "professional",
      "limit": 10
    }
  }
}
```

## Configuration

ESS runtime state is stored under `~/.ess/`:

- `~/.ess/ess.db` SQLite database
- `~/.ess/index/` Tantivy index directory

Installer-created template config file (`~/.ess/config.toml`):

```toml
[general]
default_scope = "all"

[accounts]
# [accounts.work]
# account_id = "you@company.com"
# email = "you@company.com"
# type = "professional"
# tenant_id = "your-tenant-id"
```

Graph sync credentials are read from environment variables or account config JSON:

- `ESS_TENANT_ID`
- `ESS_CLIENT_ID`
- `ESS_CLIENT_SECRET`
- Optional overrides:
  - `ESS_GRAPH_TOKEN_URL`
  - `ESS_GRAPH_API_BASE`

## Multi-account setup

Add multiple accounts:

```bash
ess accounts add work@company.com professional --tenant-id <tenant-id>
ess accounts add personal@gmail.com personal
```

Run commands across all accounts or target one:

```bash
ess sync
ess sync --account work@company.com
ess search "invoice" --scope pro
ess list --scope personal
```

## Scope filtering

`--scope` controls account-type filtering:

- `all`: no account-type filter
- `pro`: only `professional` accounts
- `personal`: only `personal` accounts

Examples:

```bash
ess search "travel" --scope personal
ess list --scope pro --unread
ess stats --scope all
```

## Architecture

```text
                +--------------------+
                |  Graph API / JSON  |
                |     Connectors     |
                +---------+----------+
                          |
                          v
+-------------+    upsert/query    +------------------+
|    CLI /    +------------------->| SQLite (~/.ess)  |
| MCP Server  |                    |  canonical store |
+------+------+                    +---------+--------+
       |                                     |
       | search/reindex                      | reindex/source-of-truth
       v                                     v
+------+------------------------------+  +--------------------------+
| Tantivy index (~/.ess/index)        |  | Contacts + sync_state    |
| subject/from/body full-text search  |  | account stats/state keys |
+-------------------------------------+  +--------------------------+
```

Primary modules:
- `src/main.rs`: CLI dispatch
- `src/connectors/`: Graph + JSON import connectors
- `src/db/`: SQLite models, schema, query APIs
- `src/indexer/`: Tantivy indexing and search
- `src/mcp/`: MCP stdio server and tools

## Contributing

### Development loop

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo run -- --help
```

### Suggested validation before PR

- Run unit + integration tests.
- Verify CLI text and `--json` mode.
- Validate MCP `initialize`, `tools/list`, and at least one `tools/call`.
- For search/index changes, run `ess reindex` and a smoke search.

### Notes

- Keep logging on stderr so JSON outputs remain parseable.
- Preserve UTF-8-safe snippet handling in search formatting.
