use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::db::schema;

const SCHEMA_VERSION_KEY: &str = "schema_version";
const LATEST_SCHEMA_VERSION: u32 = 1;

pub fn migrate(conn: &Connection) -> Result<()> {
    ensure_sync_state_table(conn)?;

    let current_version = current_schema_version(conn)?;
    if current_version > LATEST_SCHEMA_VERSION {
        return Err(anyhow!(
            "database schema version {current_version} is newer than supported version {LATEST_SCHEMA_VERSION}"
        ));
    }

    if current_version < 1 {
        apply_v1(conn)?;
    }

    Ok(())
}

fn ensure_sync_state_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sync_state (
            key TEXT PRIMARY KEY,
            value TEXT,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );
        "#,
    )
    .context("ensure sync_state table for migration tracking")?;

    Ok(())
}

fn current_schema_version(conn: &Connection) -> Result<u32> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM sync_state WHERE key = ?1 LIMIT 1",
            params![SCHEMA_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()
        .context("read current schema version from sync_state")?;

    match raw {
        None => Ok(0),
        Some(version) => version
            .parse::<u32>()
            .with_context(|| format!("invalid schema version in database: {version}")),
    }
}

fn set_schema_version(conn: &Connection, version: u32) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO sync_state (key, value, updated_at)
        VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at
        "#,
        params![SCHEMA_VERSION_KEY, version.to_string()],
    )
    .with_context(|| format!("set schema version to {version}"))?;

    Ok(())
}

fn apply_v1(conn: &Connection) -> Result<()> {
    schema::create_schema(conn).context("apply schema migration v1")?;
    set_schema_version(conn, 1)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use rusqlite::Connection;
    use uuid::Uuid;

    use super::{current_schema_version, migrate};

    fn temp_db_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("ess-migrations-{}.db", Uuid::new_v4()));
        path
    }

    #[test]
    fn migrate_sets_v1_for_fresh_database() -> Result<()> {
        let db_path = temp_db_path();
        let conn = Connection::open(&db_path)?;

        migrate(&conn)?;
        assert_eq!(current_schema_version(&conn)?, 1);

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }

    #[test]
    fn migrate_is_idempotent_for_existing_database() -> Result<()> {
        let db_path = temp_db_path();
        let conn = Connection::open(&db_path)?;

        migrate(&conn)?;
        let first_version = current_schema_version(&conn)?;
        migrate(&conn)?;
        let second_version = current_schema_version(&conn)?;

        assert_eq!(first_version, 1);
        assert_eq!(second_version, 1);

        let _ = std::fs::remove_file(db_path);
        Ok(())
    }
}
