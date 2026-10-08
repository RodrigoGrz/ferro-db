//! End-to-end demo: opens (or creates) `data.db`, creates the `accounts` table on the first
//! run, adds one row per run and prints the catalog and the rows. Run it twice to see the
//! data survive.

use std::error::Error;

use ferro_db::catalog::{Catalog, CatalogError, TableInfo};
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::storage::heap_file::HeapFile;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

const DATABASE_FILE: &str = "data.db";

fn main() -> Result<(), Box<dyn Error>> {
    let mut bpm = BufferPoolManager::new(8, PAGE_SIZE, DATABASE_FILE)?;
    let mut catalog = match Catalog::open(&mut bpm) {
        Err(err @ CatalogError::NotADatabase { .. }) => {
            eprintln!(
                "{DATABASE_FILE} was written by an older ferroDB format; delete it and run again"
            );
            return Err(err.into());
        }
        result => result?,
    };

    let accounts = accounts_table(&mut catalog, &mut bpm)?;
    let mut heap = HeapFile::open(&mut bpm, accounts.first_page_id)?;
    let next_id = heap.scan(&mut bpm).count() + 1;
    let row = new_account(i32::try_from(next_id)?);
    heap.insert(&mut bpm, &accounts.schema.encode_row(&row)?)?;

    println!("tables:");
    for table in catalog.tables() {
        let columns: Vec<String> = table.schema.columns().iter().map(describe).collect();
        println!("  {} ({})", table.name, columns.join(", "));
    }

    println!("rows of {}:", accounts.name);
    for entry in heap.scan(&mut bpm) {
        let (rid, tuple) = entry?;
        let values: Vec<String> = accounts
            .schema
            .decode_row(&tuple)?
            .iter()
            .map(Value::to_string)
            .collect();
        println!("  {rid} -> ({})", values.join(", "));
    }
    Ok(())
}

fn accounts_table(
    catalog: &mut Catalog,
    bpm: &mut BufferPoolManager,
) -> Result<TableInfo, Box<dyn Error>> {
    match catalog.table("accounts") {
        Ok(table) => Ok(table.clone()),
        Err(CatalogError::UndefinedTable { .. }) => {
            let schema = Schema::new(vec![
                Column::not_null("id", DataType::Integer),
                Column::new("age", DataType::SmallInt),
                Column::new("balance", DataType::BigInt),
            ])?;
            Ok(catalog.create_table(bpm, "accounts", schema)?.clone())
        }
        Err(err) => Err(err.into()),
    }
}

/// Every third account has no age, to show NULL.
fn new_account(id: i32) -> Vec<Value> {
    let age = if id % 3 == 0 {
        Value::Null
    } else {
        Value::SmallInt(18 + (id % 50) as i16)
    };
    vec![
        Value::Integer(id),
        age,
        Value::BigInt(i64::from(id) * 1_000),
    ]
}

fn describe(column: &Column) -> String {
    let null = if column.nullable { "" } else { " NOT NULL" };
    format!("{} {}{null}", column.name, column.data_type.name())
}
