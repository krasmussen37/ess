use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::db::models::Account;
use crate::db::Database;
use crate::indexer::EmailIndex;

pub mod graph_api;
pub mod json_archive;

pub use graph_api::GraphApiConnector;
pub use json_archive::JsonArchiveConnector;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub emails_added: usize,
    pub emails_updated: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub files_processed: usize,
    pub emails_imported: usize,
    pub errors: Vec<String>,
}

#[async_trait(?Send)]
pub trait EmailConnector: Send + Sync {
    fn name(&self) -> &str;

    async fn sync(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
    ) -> Result<SyncReport>;

    async fn import(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        path: &Path,
        account: &Account,
    ) -> Result<ImportReport>;
}

pub struct ConnectorRegistry {
    connectors: Vec<Box<dyn EmailConnector>>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self {
            connectors: Vec::new(),
        }
    }

    pub fn register(&mut self, connector: Box<dyn EmailConnector>) {
        self.connectors.push(connector);
    }

    pub fn by_name(&self, name: &str) -> Option<&dyn EmailConnector> {
        self.connectors
            .iter()
            .find(|connector| connector.name().eq_ignore_ascii_case(name))
            .map(|connector| connector.as_ref())
    }

    pub fn all(&self) -> &[Box<dyn EmailConnector>] {
        &self.connectors
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;

    use super::{ConnectorRegistry, EmailConnector, ImportReport, SyncReport};
    use crate::db::models::Account;
    use crate::db::Database;
    use crate::indexer::EmailIndex;

    struct DummyConnector;

    #[async_trait(?Send)]
    impl EmailConnector for DummyConnector {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn sync(
            &self,
            _db: &Database,
            _indexer: &mut EmailIndex,
            _account: &Account,
        ) -> Result<SyncReport> {
            Ok(SyncReport::default())
        }

        async fn import(
            &self,
            _db: &Database,
            _indexer: &mut EmailIndex,
            _path: &std::path::Path,
            _account: &Account,
        ) -> Result<ImportReport> {
            Ok(ImportReport::default())
        }
    }

    #[test]
    fn reports_default_to_zero_counts() {
        assert_eq!(SyncReport::default().emails_added, 0);
        assert_eq!(ImportReport::default().files_processed, 0);
    }

    #[test]
    fn connector_trait_is_object_safe() {
        let connector: Box<dyn EmailConnector> = Box::new(DummyConnector);
        assert_eq!(connector.name(), "dummy");
    }

    #[test]
    fn registry_registers_and_finds_connectors() {
        let mut registry = ConnectorRegistry::new();
        registry.register(Box::new(DummyConnector));
        assert_eq!(registry.all().len(), 1);
        assert!(registry.by_name("dummy").is_some());
        assert!(registry.by_name("missing").is_none());
    }
}
