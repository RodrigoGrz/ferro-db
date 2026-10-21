//! A database in a temporary file for the executors' tests: a catalog and a buffer pool with
//! small pages, so tables span pages quickly.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::Catalog;
use crate::storage::buffer_pool::BufferPoolManager;
use crate::table::{Schema, TableHeap};
use crate::types::Value;

use super::{ExecutionContext, Row};

const PAGE_SIZE: usize = 512;

pub(super) struct TestDb {
    path: PathBuf,
    bpm: Option<BufferPoolManager>,
    pub catalog: Catalog,
}

impl TestDb {
    pub fn new(pool_size: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ferro-exec-{}-{id}.db", std::process::id()));
        let mut bpm = BufferPoolManager::new(pool_size, PAGE_SIZE, &path).unwrap();
        let catalog = Catalog::open(&mut bpm).unwrap();
        Self {
            path,
            bpm: Some(bpm),
            catalog,
        }
    }

    /// Runs `change` on a table's heap, behind the executors' back.
    pub fn with_table<T>(
        &mut self,
        name: &str,
        change: impl FnOnce(&mut TableHeap, &mut BufferPoolManager) -> T,
    ) -> T {
        let bpm = self.bpm.as_mut().unwrap();
        let heap = self.catalog.table_heap(bpm, name).unwrap();
        change(heap, bpm)
    }

    /// What the table holds, read straight from its heap.
    pub fn stored_rows(&mut self, table: &str) -> Vec<Row> {
        self.with_table(table, |heap, bpm| {
            heap.scan(bpm)
                .map(|entry| {
                    let (rid, values) = entry.unwrap();
                    Row {
                        values,
                        rid: Some(rid),
                    }
                })
                .collect()
        })
    }

    pub fn ctx(&mut self) -> ExecutionContext<'_> {
        ExecutionContext {
            catalog: &mut self.catalog,
            bpm: self.bpm.as_mut().unwrap(),
        }
    }

    /// Drops the pool (flushing every page) and opens the file and the catalog again.
    pub fn reopen(&mut self, pool_size: usize) {
        self.bpm.take();
        let mut bpm = BufferPoolManager::new(pool_size, PAGE_SIZE, &self.path).unwrap();
        self.catalog = Catalog::open(&mut bpm).unwrap();
        self.bpm = Some(bpm);
    }

    pub fn create_table(&mut self, name: &str, schema: Schema, rows: &[Vec<Value>]) {
        let bpm = self.bpm.as_mut().unwrap();
        self.catalog.create_table(bpm, name, schema).unwrap();
        for row in rows {
            let heap = self.catalog.table_heap(bpm, name).unwrap();
            heap.insert(bpm, row).unwrap();
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        self.bpm.take();
        let _ = fs::remove_file(&self.path);
    }
}
