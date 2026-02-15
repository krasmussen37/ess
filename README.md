# ESS (Email Search Service)

![Rust](https://img.shields.io/badge/rust-2021-orange)
![License](https://img.shields.io/badge/license-MIT-blue)
![Storage](https://img.shields.io/badge/storage-SQLite%20%2B%20Tantivy-4c8)

ESS is a local-first email search service with a CLI and an MCP server. It stores canonical email data in SQLite and indexes searchable content in Tantivy.

**Who it's for:** Developers and AI agents that need programmatic access to email data without cloud dependencies or third-party SaaS.

**Why ESS:**
- **Local-first** — your email data stays on your machine in SQLite + Tantivy, not in someone else's cloud
- **MCP-native** — five tools (`ess_search`, `ess_thread`, `ess_contacts`, `ess_recent`, `ess_stats`) ready for any MCP client
- **Fast full-text search** — Tantivy provides sub-second search across thousands of emails
- **Multi-account** — manage professional and personal accounts with scope filtering (`--scope pro`)
- **Flexible ingest** — import JSON archives or sync live from Microsoft Graph with delta tokens

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

## Example output

### `ess stats`

```
ESS Stats
=========
Accounts: 1
Emails:   26
Contacts: 13

Emails by account
-----------------
test@example.com               26

Index Docs: 26
Index Size (bytes): 6404508
```

### `ess stats --json`

```json
{
  "database": {
    "total_accounts": 1,
    "total_emails": 26,
    "total_contacts": 13,
    "emails_by_account": [
      { "account_id": "test@example.com", "count": 26 }
    ]
  },
  "index_doc_count": 26,
  "index_size_bytes": 6404508
}
```

### `ess list --limit 3`

```
From                      Subject                                                   Date
------------------------  --------------------------------------------------------  ----------
Claude Team               Introducing Claude Opus 4.6 and agent teams               2026-02-05
Anthropic                 Secure link to log in to Claude.ai                         2026-02-01
Claude Team               Welcome to Claude — let's get started                     2026-01-30
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

## JSON archive format

ESS imports `.json` files from a directory. Each file represents one email. The connector accepts both Microsoft Graph API format and a simpler flat format.

### Minimal example

```json
{
  "id": "msg-001",
  "subject": "Q2 Planning Kickoff",
  "receivedDateTime": "2026-01-15T10:30:00Z",
  "from": { "name": "Alice Chen", "address": "alice@example.com" },
  "toRecipients": [
    { "name": "Bob Smith", "address": "bob@example.com" }
  ],
  "body": { "contentType": "text", "content": "Let's schedule the kickoff for next week." },
  "bodyPreview": "Let's schedule the kickoff for next week."
}
```

### Field reference

| Field | Required | Description |
|-------|----------|-------------|
| `id` | yes | Unique message identifier |
| `subject` | no | Email subject line |
| `receivedDateTime` | no | ISO 8601 timestamp (falls back to `sentDateTime`, then current time) |
| `sentDateTime` | no | When the email was sent |
| `from` | no | Object with `name` and/or `address` (or Graph format with `emailAddress.name`/`emailAddress.address`) |
| `toRecipients` | no | Array of recipient objects (same format as `from`) |
| `ccRecipients` | no | Array of CC recipients |
| `bccRecipients` | no | Array of BCC recipients |
| `body` | no | Object with `contentType` (`"text"` or `"html"`) and `content`, or a plain string |
| `bodyPreview` | no | Short text preview of the body |
| `importance` | no | `"low"`, `"normal"`, or `"high"` |
| `isRead` | no | Boolean |
| `hasAttachments` | no | Boolean |
| `headers` | no | Object with MIME headers (`Message-ID`, `Thread-Topic`, etc.) |
| `conversationId` | no | Thread/conversation grouping ID |
| `internetMessageId` | no | RFC 2822 Message-ID |
| `categories` | no | Array of category strings |
| `webLink` | no | URL to the message in a web client |

Fields can be nested under an `"email"` wrapper object. The connector also reads MIME headers (`From`, `To`, `Cc`, `Bcc`, `Message-ID`, `Thread-Topic`) as fallbacks when top-level fields are missing.

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

## See also

- [ESM (Email Search Memory)](https://github.com/krasmussen37/ess-memory) — pattern engine companion. ESM extracts communication patterns from ESS search results and uses them to improve outbound email. Use `esm reflect` to mine patterns from ESS data, and `esm context` to get drafting guidance before writing.

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
