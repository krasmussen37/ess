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
- **Flexible ingest** — import JSON archives or sync live from Microsoft Graph and Gmail APIs

## What ESS does

- Imports JSON email archives into a local SQLite database.
- Syncs from Microsoft Graph and Gmail APIs (delta sync with token caching).
- Indexes email text for fast full-text search.
- Exposes both CLI commands and MCP tools (`ess_search`, `ess_thread`, `ess_contacts`, `ess_recent`, `ess_stats`).
- Supports multi-account setups with account-type scoping (`professional`, `personal`).

### Graph folder coverage

Graph sync dynamically discovers all mailbox folders via `GET /users/{email}/mailFolders` (including hidden folders). Every discovered folder is synced using delta queries, so custom folders, subfolder hierarchies, and folders created by third-party clients (e.g. Superhuman's "Done" → Archive) are all captured.

Well-known folders are normalised to short ESS labels:

| Display name | ESS `emails.folder` |
|---|---|
| Inbox | `inbox` |
| Sent Items | `sent` |
| Archive | `archive` |
| Drafts | `drafts` |
| Deleted Items | `trash` |
| Junk Email | `spam` |
| Outbox | `outbox` |
| Conversation History | `conversation_history` |

Custom/user-created folders use their lowercased display name as the label. Child folders use a `parent/child` path format.

System folders (`Sync Issues`, `Conflicts`, `Local Failures`, `Server Failures`) and search folders are excluded automatically.

Each folder has its own delta cursor in `sync_state` keyed by folder ID:
- `graph_delta_link:{account_id}:{folder_id}`

Legacy delta cursors (well-known name keys and pre-multi-folder inbox keys) are migrated automatically on next sync.

### Sync strategy

Initial sync uses the Graph `/messages` endpoint to enumerate all messages in each folder, then establishes a delta baseline for future incremental syncs. Subsequent syncs use delta queries, which are fast (typically seconds) and only fetch new/changed/deleted messages.

Meeting invite notifications (subjects like "Updated invitation: ...", "Accepted: ...") are `eventMessage` types in the Graph API. They inherit from `message` and are returned by `/messages`, so ESS syncs them alongside regular email. Calendar **events** (structured start/end times, attendees, RSVP) are separate Graph API resources and are not part of email sync.

### Item count discrepancy

The Graph API `totalItemCount` on a folder counts **all item types** (emails, calendar items, tasks, FAI), not just messages. The `/messages` endpoint returns only email-type items. This means `totalItemCount` will typically exceed the actual number of synced emails. Additionally, the `Deleted Items` folder's `totalItemCount` includes Recoverable Items (soft-deleted dumpster) which are not accessible via the Graph API.

ESS uses `/messages` pagination count as the source of truth, not `totalItemCount`.

### Known limitations

- The Exchange Online In-Place Archive (Online Archive mailbox) is not accessible via Microsoft Graph API (v1.0 or beta). This is a Microsoft platform limitation. Superhuman's "Done" action uses the primary mailbox's Archive folder, which is synced normally.
- Recoverable Items in `Deleted Items` (permanently deleted items still in retention hold) are not accessible via the Graph API and are not synced.

## Prerequisites

- Rust toolchain (`cargo`, Rust 1.75+ recommended)
- Linux/macOS shell
- Optional for Graph sync:
  - Microsoft Graph app credentials (`ESS_CLIENT_ID`, `ESS_CLIENT_SECRET`, `ESS_TENANT_ID`)
- Optional for Gmail sync:
  - Google OAuth credentials (`ESS_GMAIL_CLIENT_ID`, `ESS_GMAIL_CLIENT_SECRET`, `ESS_GMAIL_REFRESH_TOKEN`) or per-account `--config` JSON

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
Accounts: 2
Emails:   12500
Contacts: 1830

Emails by account
-----------------
you@company.com                10200
personal@gmail.com              2300

Index Docs: 12500
Index Size (bytes): 536870912
```

### `ess stats --json`

```json
{
  "database": {
    "total_accounts": 2,
    "total_emails": 12500,
    "total_contacts": 1830,
    "emails_by_account": [
      { "account_id": "you@company.com", "count": 10200 },
      { "account_id": "personal@gmail.com", "count": 2300 }
    ]
  },
  "index_doc_count": 12500,
  "index_size_bytes": 536870912
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

Sync configured accounts from Microsoft Graph and Gmail.

Examples:
```bash
# Sync all accounts
ess sync

# Sync from Microsoft Graph
ess sync --account work@company.com

# Sync from Gmail
ess sync --account personal@gmail.com

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

A reference `.mcp.json` is included in the repo as a starting point. Add ESS to your MCP client config:

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

## Sync best practices

### Initial sync / archive build-up

When populating ESS for the first time with real email data:

**Run syncs sequentially, one account at a time.** The Tantivy search index only supports one writer process. Running two `ess sync` commands concurrently will cause the second to fail with an index lock error.

```bash
# Correct: sequential
ess sync --account work@company.com
ess sync --account personal@gmail.com

# Wrong: concurrent (will fail)
ess sync --account work@company.com &
ess sync --account personal@gmail.com &  # index lock error
```

**Gmail initial syncs are slow for large mailboxes.** The Gmail API requires one HTTP request per message during full sync. A mailbox with 20,000 emails will take a while. ESS refreshes the OAuth token automatically during long syncs, so token expiry is handled. Monitor progress with:

```bash
ess stats --json  # check email counts while sync runs
```

**Interrupted syncs are safe to restart.** SQLite upserts prevent duplicate emails. If a sync fails partway through, simply re-run it. The sync will re-enumerate messages but skip those already stored. However, for Gmail, the `historyId` watermark isn't saved until the full sync completes, so restarts redo the full `messages.list` enumeration.

**Per-account credentials go in `--config` JSON.** When different accounts use different OAuth apps (e.g., two Gmail accounts from different Google Cloud projects), pass per-account credentials via `--config`:

```bash
ess accounts add user@gmail.com personal \
  --config '{"connector":"gmail_api","client_id":"...","client_secret":"...","refresh_token":"..."}'
```

Credentials are stored in the local SQLite database (`~/.ess/ess.db`), never committed to git.

**Rebuild the index after problems.** If a sync was killed mid-write or the index shows corruption (merge errors, missing segments), rebuild from SQLite:

```bash
rm -rf ~/.ess/index
ess reindex
```

SQLite is the source of truth. The index can always be rebuilt.

### Ongoing sync and indexing

After the initial load completes:

**Delta syncs are fast.** Gmail uses `historyId` and Graph uses delta tokens for incremental sync. Only new/changed/deleted messages are fetched. A typical delta sync takes seconds.

**Graph keeps one delta token per folder.** This prevents cross-folder cursor conflicts and allows inbox/sent/archive/drafts/trash/spam to advance independently.

**Use `--watch` for automatic periodic syncing:**

```bash
ess sync --watch  # polls for changes on a timer
```

**Never run multiple sync processes simultaneously.** The Tantivy index writer is exclusive. Use `ess sync` (no `--account` flag) to sync all accounts sequentially in one process.

**If search results seem stale or incomplete,** rebuild the index:

```bash
ess reindex  # rebuilds from SQLite, no data loss
```

**Expired delta tokens trigger automatic fallback.** If a Gmail `historyId` or Graph delta token expires (too long between syncs), ESS falls back to a full sync automatically. A warning is logged but no manual intervention is needed.

**Index sizing:** Expect roughly 0.3-0.5 GB of index per 1,000 emails (varies with email body sizes). A 20K email corpus produces a ~6-9 GB Tantivy index.

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
                |  Graph API / Gmail |
                |  API / JSON        |
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
- `src/connectors/`: Graph API, Gmail API, and JSON import connectors
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
- The `.gitignore` includes common build and runtime artifacts you should expect when working with the repo. Review it when setting up your environment.
