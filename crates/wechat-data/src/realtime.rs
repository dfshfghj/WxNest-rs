use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wechat_broker::{BrokerClient, BrokerHost, BrokerOptions, Cell};

use crate::error::Result;
use crate::sqlite::SqlValue;

pub struct RealtimeHost {
    host: Arc<BrokerHost>,
}

impl RealtimeHost {
    pub fn spawn(broker_exe: &Path, roots: &[PathBuf]) -> Result<Self> {
        Self::spawn_with_options(broker_exe, roots, &BrokerOptions::default())
    }

    pub fn spawn_with_options(
        broker_exe: &Path,
        roots: &[PathBuf],
        options: &BrokerOptions,
    ) -> Result<Self> {
        Ok(Self {
            host: Arc::new(BrokerHost::spawn_with_options(broker_exe, roots, options)?),
        })
    }

    pub fn client(&self) -> BrokerClient {
        self.host.client()
    }

    pub fn open(&self, db_path: &Path, key_hex: &str, raw_key: bool) -> Result<RealtimeDatabase> {
        RealtimeDatabase::with_host(
            self.client(),
            db_path,
            key_hex,
            raw_key,
            Some(Arc::clone(&self.host)),
        )
    }
}

pub struct RealtimeDatabase {
    _host: Option<Arc<BrokerHost>>,
    client: BrokerClient,
    db_id: u64,
    closed: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct RealtimePage {
    pub columns: Vec<String>,
    pub rows: Vec<BTreeMap<String, SqlValue>>,
    pub truncated: bool,
}

impl RealtimePage {
    pub fn rows_as_maps(&self) -> Vec<BTreeMap<String, SqlValue>> {
        self.rows.clone()
    }
}

impl RealtimeDatabase {
    pub fn open(
        broker_exe: &Path,
        root: &Path,
        db_path: &Path,
        key_hex: &str,
        raw_key: bool,
    ) -> Result<Self> {
        let roots = [root.to_path_buf()];
        let host = Arc::new(BrokerHost::spawn(broker_exe, &roots)?);
        Self::with_host(host.client(), db_path, key_hex, raw_key, Some(host))
    }

    pub fn open_with_client(
        client: BrokerClient,
        db_path: &Path,
        key_hex: &str,
        raw_key: bool,
    ) -> Result<Self> {
        Self::with_host(client, db_path, key_hex, raw_key, None)
    }

    fn with_host(
        client: BrokerClient,
        db_path: &Path,
        key_hex: &str,
        raw_key: bool,
        host: Option<Arc<BrokerHost>>,
    ) -> Result<Self> {
        let db_id = client.open(db_path, key_hex, raw_key)?;
        Ok(Self {
            _host: host,
            client,
            db_id,
            closed: AtomicBool::new(false),
        })
    }

    pub fn query(&self, sql: &str, max_rows: usize) -> Result<RealtimePage> {
        self.query_with_params(sql, &[], max_rows)
    }

    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[wechat_broker::Cell],
        max_rows: usize,
    ) -> Result<RealtimePage> {
        let page = self
            .client
            .query_with_params(self.db_id, sql, params, max_rows)?;
        let rows = page
            .rows_as_maps()
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(column, cell)| (column, cell_to_sql(cell)))
                    .collect()
            })
            .collect();
        Ok(RealtimePage {
            columns: page.columns,
            rows,
            truncated: page.truncated,
        })
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.client.close(self.db_id);
        }
    }
}

impl Drop for RealtimeDatabase {
    fn drop(&mut self) {
        self.close();
    }
}

fn cell_to_sql(cell: Cell) -> SqlValue {
    match cell {
        Cell::Null => SqlValue::Null,
        Cell::Integer(value) => SqlValue::Integer(value),
        Cell::Real(value) => SqlValue::Real(value),
        Cell::Text(value) => SqlValue::Text(value),
        Cell::Blob(value) => SqlValue::Blob(value),
    }
}
