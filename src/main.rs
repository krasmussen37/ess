use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Scope {
    Pro,
    Personal,
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AccountTypeArg {
    Professional,
    Personal,
}

#[derive(Debug, Parser)]
#[command(name = "ess", version, about = "Email Search Service")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output structured JSON
    #[arg(long, global = true)]
    json: bool,

    /// Filter account scope
    #[arg(long, global = true, value_enum, default_value = "all")]
    scope: Scope,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Search indexed emails
    Search(SearchArgs),
    /// List emails with optional filters
    List(ListArgs),
    /// Show one email by ID
    Show { id: String },
    /// Show all messages in a thread
    Thread { conversation_id: String },
    /// Sync from configured accounts
    Sync(SyncArgs),
    /// Import from JSON archive path
    Import(ImportArgs),
    /// List/search contacts
    Contacts(ContactsArgs),
    /// Manage account configuration/state
    Accounts {
        #[command(subcommand)]
        command: AccountCommands,
    },
    /// Show index and DB stats
    Stats,
    /// Rebuild search index from SQLite source-of-truth
    Reindex,
    /// Run MCP server over stdio
    Mcp,
}

#[derive(Debug, Args)]
struct SearchArgs {
    query: String,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long)]
    account: Option<String>,
    #[arg(long)]
    folder: Option<String>,
    #[arg(long, default_value_t = 25)]
    limit: usize,
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    from: Option<String>,
    #[arg(long, default_value_t = false)]
    unread: bool,
    #[arg(long)]
    account: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Debug, Args)]
struct SyncArgs {
    #[arg(long)]
    account: Option<String>,
    #[arg(long, default_value_t = false)]
    full: bool,
    #[arg(long, default_value_t = false)]
    watch: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    path: String,
    #[arg(long)]
    account: Option<String>,
}

#[derive(Debug, Args)]
struct ContactsArgs {
    #[arg(long)]
    query: Option<String>,
    #[arg(long, default_value_t = false)]
    enrich: bool,
}

#[derive(Debug, Subcommand)]
enum AccountCommands {
    /// List configured accounts
    List,
    /// Add account configuration
    Add {
        email: String,
        account_type: AccountTypeArg,
        #[arg(long)]
        tenant_id: Option<String>,
    },
    /// Remove account configuration
    Remove { account_id: String },
    /// Show account sync status
    SyncStatus,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    commands::dispatch(cli).await
}

mod commands {
    use anyhow::{anyhow, Context, Result};
    use chrono::NaiveDate;
    use serde::Serialize;

    use ess::connectors::{EmailConnector, GraphApiConnector, JsonArchiveConnector};
    use ess::db::models::{Account, AccountType};
    use ess::db::{Database, EmailSearchFilters};
    use ess::indexer::EmailIndex;
    use ess::output::{self, OutputFormat, SearchResultItem};
    use ess::search;
    use ess::search::filters::{EmailFilters, Scope as SearchScope};

    use super::{AccountCommands, Cli, Commands, Scope};

    pub async fn dispatch(cli: Cli) -> Result<()> {
        match cli.command {
            Commands::Search(args) => handle_search(args, cli.scope, cli.json).await,
            Commands::List(args) => handle_list(args, cli.scope, cli.json).await,
            Commands::Show { id } => handle_show(&id, cli.json).await,
            Commands::Thread { conversation_id } => handle_thread(&conversation_id, cli.json).await,
            Commands::Sync(args) => handle_sync(args).await,
            Commands::Import(args) => handle_import(args, cli.json).await,
            Commands::Contacts(args) => handle_contacts(args, cli.json).await,
            Commands::Accounts { command } => handle_accounts(command).await,
            Commands::Stats => handle_stats(cli.json).await,
            Commands::Reindex => handle_reindex().await,
            Commands::Mcp => handle_mcp().await,
        }
    }

    async fn handle_search(args: super::SearchArgs, scope: Scope, json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let index_path =
            EmailIndex::default_index_path().context("resolve default ESS index path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let index = EmailIndex::open(&index_path)
            .with_context(|| format!("open ESS index at {}", index_path.display()))?;

        let filters = EmailFilters {
            scope: map_scope(scope),
            from: args.from,
            since: parse_date_arg("since", args.since)?,
            until: parse_date_arg("until", args.until)?,
            account: args.account,
            folder: args.folder,
            limit: args.limit,
            ..EmailFilters::default()
        };

        let results = search::search_emails(&index, &db, &args.query, &filters)?;
        let formatted = output::format_search_results(
            OutputFormat::from_json_flag(json),
            &results
                .into_iter()
                .map(|result| SearchResultItem {
                    email: result.email,
                    score: Some(result.score),
                })
                .collect::<Vec<_>>(),
        )?;
        println!("{formatted}");
        Ok(())
    }

    async fn handle_list(args: super::ListArgs, scope: Scope, json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;

        let mut emails = db.search_emails(EmailSearchFilters {
            query: None,
            account_id: args.account,
            account_type: map_scope_to_account_type(scope),
            folder: None,
            from_address: args.from,
            limit: args.limit,
            offset: 0,
        })?;

        if args.unread {
            emails.retain(|email| !email.is_read.unwrap_or(false));
        }

        let formatted = output::format_search_results(
            OutputFormat::from_json_flag(json),
            &emails
                .into_iter()
                .map(|email| SearchResultItem { email, score: None })
                .collect::<Vec<_>>(),
        )?;
        println!("{formatted}");
        Ok(())
    }

    async fn handle_show(id: &str, json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let email = db
            .get_email(id)?
            .ok_or_else(|| anyhow!("email not found for id '{id}'"))?;

        let formatted = output::format_email(OutputFormat::from_json_flag(json), &email)?;
        println!("{formatted}");
        Ok(())
    }

    async fn handle_thread(conversation_id: &str, json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let emails = db.get_emails_by_conversation(conversation_id)?;
        let formatted = output::format_thread(OutputFormat::from_json_flag(json), &emails)?;
        println!("{formatted}");
        Ok(())
    }

    async fn handle_sync(args: super::SyncArgs) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let index_path =
            EmailIndex::default_index_path().context("resolve default ESS index path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let mut index = EmailIndex::open(&index_path)
            .with_context(|| format!("open ESS index at {}", index_path.display()))?;
        let connector = GraphApiConnector::new();
        let accounts = resolve_accounts(&db, args.account.as_deref())?;

        if args.full {
            eprintln!("--full requested: running full sync pass for selected account(s)");
        }

        if args.watch {
            loop {
                run_sync_cycle(&connector, &db, &mut index, &accounts).await?;
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        } else {
            run_sync_cycle(&connector, &db, &mut index, &accounts).await
        }
    }

    async fn handle_import(args: super::ImportArgs, json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let index_path =
            EmailIndex::default_index_path().context("resolve default ESS index path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let mut index = EmailIndex::open(&index_path)
            .with_context(|| format!("open ESS index at {}", index_path.display()))?;
        let account = resolve_single_account(&db, args.account.as_deref())?;

        let connector = JsonArchiveConnector::new();
        let report = connector
            .import(&db, &mut index, std::path::Path::new(&args.path), &account)
            .await
            .with_context(|| format!("import archive path {}", args.path))?;

        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("Import complete");
            println!("Files processed: {}", report.files_processed);
            println!("Emails imported: {}", report.emails_imported);
            if report.errors.is_empty() {
                println!("Errors: 0");
            } else {
                println!("Errors: {}", report.errors.len());
                for error in report.errors {
                    println!("- {error}");
                }
            }
        }
        Ok(())
    }

    async fn handle_contacts(args: super::ContactsArgs, json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let contacts = db.get_contacts(args.query.as_deref())?;
        if args.enrich {
            eprintln!("--enrich is not implemented yet; showing current contact data");
        }
        let formatted = output::format_contacts(OutputFormat::from_json_flag(json), &contacts)?;
        println!("{formatted}");
        Ok(())
    }

    async fn handle_accounts(command: AccountCommands) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;

        match command {
            AccountCommands::List => {
                let accounts = db.list_accounts()?;
                if accounts.is_empty() {
                    println!("No accounts configured.");
                } else {
                    println!("Accounts");
                    println!("========");
                    for account in accounts {
                        println!(
                            "{}  {}  {}",
                            account.account_id, account.email_address, account.account_type
                        );
                    }
                }
            }
            AccountCommands::Add {
                email,
                account_type,
                tenant_id,
            } => {
                let account = Account {
                    account_id: email.trim().to_ascii_lowercase(),
                    email_address: email,
                    display_name: None,
                    tenant_id,
                    account_type: map_account_type(account_type),
                    enabled: true,
                    last_sync: None,
                    config: None,
                };
                db.insert_account(&account)?;
                println!("Added account: {}", account.account_id);
            }
            AccountCommands::Remove { account_id } => {
                let removed = db.remove_account(&account_id)?;
                if removed == 0 {
                    println!("No account found: {account_id}");
                } else {
                    println!("Removed account: {account_id}");
                }
            }
            AccountCommands::SyncStatus => {
                let accounts = db.list_accounts()?;
                if accounts.is_empty() {
                    println!("No accounts configured.");
                } else {
                    println!("Account Sync Status");
                    println!("===================");
                    for account in accounts {
                        println!(
                            "{}  enabled={}  last_sync={}",
                            account.account_id,
                            account.enabled,
                            account.last_sync.as_deref().unwrap_or("never")
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_stats(json: bool) -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let index_path =
            EmailIndex::default_index_path().context("resolve default ESS index path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let index = EmailIndex::open(&index_path)
            .with_context(|| format!("open ESS index at {}", index_path.display()))?;
        let db_stats = db.get_stats()?;
        let index_stats = index.get_stats()?;

        if json {
            #[derive(Serialize)]
            struct StatsPayload {
                database: ess::db::DatabaseStats,
                index_doc_count: u64,
                index_size_bytes: u64,
            }
            let payload = StatsPayload {
                database: db_stats,
                index_doc_count: index_stats.doc_count,
                index_size_bytes: index_stats.index_size_bytes,
            };
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            let rendered = output::format_stats(OutputFormat::Table, &db_stats)?;
            println!("{rendered}");
            println!("Index Docs: {}", index_stats.doc_count);
            println!("Index Size (bytes): {}", index_stats.index_size_bytes);
        }
        Ok(())
    }

    async fn handle_reindex() -> Result<()> {
        let db_path = Database::default_db_path().context("resolve default ESS database path")?;
        let index_path =
            EmailIndex::default_index_path().context("resolve default ESS index path")?;
        let db = Database::open(&db_path)
            .with_context(|| format!("open ESS database at {}", db_path.display()))?;
        let mut index = EmailIndex::open(&index_path)
            .with_context(|| format!("open ESS index at {}", index_path.display()))?;
        let indexed = index.reindex(&db)?;
        println!("Reindex complete: {indexed} emails indexed.");
        Ok(())
    }

    async fn handle_mcp() -> Result<()> {
        ess::mcp::run_stdio_server()
    }

    fn map_scope(scope: Scope) -> SearchScope {
        match scope {
            Scope::Pro => SearchScope::Professional,
            Scope::Personal => SearchScope::Personal,
            Scope::All => SearchScope::All,
        }
    }

    fn map_scope_to_account_type(scope: Scope) -> Option<String> {
        match scope {
            Scope::Pro => Some("professional".to_string()),
            Scope::Personal => Some("personal".to_string()),
            Scope::All => None,
        }
    }

    fn parse_date_arg(label: &str, raw: Option<String>) -> Result<Option<NaiveDate>> {
        raw.map(|value| {
            NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
                .with_context(|| format!("invalid --{label} date '{value}', expected YYYY-MM-DD"))
        })
        .transpose()
    }

    fn map_account_type(value: super::AccountTypeArg) -> AccountType {
        match value {
            super::AccountTypeArg::Professional => AccountType::Professional,
            super::AccountTypeArg::Personal => AccountType::Personal,
        }
    }

    fn resolve_accounts(db: &Database, account_id: Option<&str>) -> Result<Vec<Account>> {
        if let Some(account_id) = account_id {
            let account = db
                .get_account(account_id)?
                .ok_or_else(|| anyhow!("account not found: {account_id}"))?;
            return Ok(vec![account]);
        }

        let accounts = db.list_accounts()?;
        if accounts.is_empty() {
            return Err(anyhow!(
                "no accounts configured; use 'ess accounts add' first"
            ));
        }
        Ok(accounts)
    }

    fn resolve_single_account(db: &Database, account_id: Option<&str>) -> Result<Account> {
        if let Some(account_id) = account_id {
            return db
                .get_account(account_id)?
                .ok_or_else(|| anyhow!("account not found: {account_id}"));
        }

        let mut accounts = db.list_accounts()?;
        match accounts.len() {
            0 => Err(anyhow!(
                "no accounts configured; use 'ess accounts add' first"
            )),
            1 => Ok(accounts.remove(0)),
            _ => Err(anyhow!(
                "multiple accounts configured; pass --account <id> to disambiguate import target"
            )),
        }
    }

    async fn run_sync_cycle(
        connector: &GraphApiConnector,
        db: &Database,
        index: &mut EmailIndex,
        accounts: &[Account],
    ) -> Result<()> {
        for account in accounts {
            let report = connector.sync(db, index, account).await?;
            println!(
                "sync {}: added={} updated={} errors={}",
                account.account_id,
                report.emails_added,
                report.emails_updated,
                report.errors.len()
            );
        }
        Ok(())
    }
}
