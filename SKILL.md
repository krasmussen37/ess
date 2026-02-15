---
name: ess
description: Local-first email search with SQLite + Tantivy. Use when finding emails, threads, contacts, or providing email context to agents via MCP.
license: MIT
metadata:
  author: krasmussen37
  version: "0.1"
allowed-tools: Bash(ess:*)
---

# ESS (Email Search Service)

Local-first email search. SQLite canonical store + Tantivy full-text index. CLI and MCP server.

## When to Use ESS

- Finding emails by keyword, sender, date, folder
- Retrieving full threads by conversation ID
- Looking up contacts and communication frequency
- Providing email context to an agent via MCP tools
- Importing JSON email archives or syncing from Microsoft Graph

## When NOT to Use ESS (Use ESM Instead)

- Extracting communication patterns or preferences
- Getting drafting guidance before writing an email
- Tracking whether a communication approach was effective

## Quick Start

```bash
# 1. Add an account
ess accounts add you@company.com professional --tenant-id <tid>

# 2. Import email data
ess import /path/to/archive --account you@company.com

# 3. Search
ess search "quarterly planning" --from alice@company.com --since 2026-01-01
```

## CLI Reference

### Search and Browse

```bash
# Full-text search with filters
ess search "budget review" --from alice@co.com --since 2026-01-01 --limit 25
ess search "invoice" --scope pro --json

# List emails (lightweight filters, no full-text)
ess list --unread --account you@co.com --limit 50

# Show single email
ess show <message-id>

# Show full thread
ess thread <conversation-id>
```

### Contacts

```bash
ess contacts --query "alice"
```

### Account Management

```bash
ess accounts list
ess accounts add you@gmail.com personal
ess accounts remove you@gmail.com
ess accounts sync-status
```

### Import and Sync

```bash
# Import JSON archive
ess import ./archive --account you@co.com

# Sync from Microsoft Graph
ess sync
ess sync --account you@co.com
ess sync --watch
```

### Stats and Maintenance

```bash
ess stats          # DB + index summary
ess stats --json   # Machine-readable
ess reindex        # Rebuild Tantivy from SQLite
```

### Global Flags

- `--json` — machine-readable JSON output
- `--scope <pro|personal|all>` — filter by account type

## MCP Tools

Start the server: `ess mcp`

### ess_search

Full-text search with filters.

```json
{
  "name": "ess_search",
  "arguments": {
    "query": "security review",
    "scope": "professional",
    "from": "alice@company.com",
    "since": "2026-01-01",
    "limit": 10
  }
}
```

### ess_thread

Fetch all messages in a conversation.

```json
{
  "name": "ess_thread",
  "arguments": {
    "conversation_id": "AAQkAG..."
  }
}
```

### ess_contacts

Search contacts by name or email.

```json
{
  "name": "ess_contacts",
  "arguments": {
    "query": "alice"
  }
}
```

### ess_recent

List most recent emails with optional filters.

```json
{
  "name": "ess_recent",
  "arguments": {
    "scope": "professional",
    "unread_only": true,
    "limit": 20
  }
}
```

### ess_stats

Database and index summary (no arguments).

```json
{
  "name": "ess_stats",
  "arguments": {}
}
```

## Integration with ESM

ESS provides the raw email data; ESM extracts patterns from it.

```bash
# 1. Search for relevant emails in ESS
ess search "client follow-up emails" --json

# 2. Use ESM to extract communication patterns
esm reflect "client follow-up emails"

# 3. Before drafting, get context from ESM
esm context "prepare follow-up to client about project status"
```

## Troubleshooting

- **Empty search results**: Run `ess stats` to verify emails are indexed. Try `ess reindex` if index count doesn't match DB count.
- **Import skipping files**: Archive files must be `.json`. Run with `RUST_LOG=debug` for detailed import logs.
- **Graph sync auth errors**: Verify `ESS_CLIENT_ID`, `ESS_CLIENT_SECRET`, `ESS_TENANT_ID` environment variables.
- **MCP not responding**: Ensure `ess mcp` is started. Send `{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}` to verify.
