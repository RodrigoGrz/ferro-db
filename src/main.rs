//! End-to-end demo: opens (or creates) `data.db`, creates the `accounts` table on the first
//! run, adds one row per run and prints the catalog and the rows. Run it twice to see the
//! data survive.

use std::error::Error;

use ferro_db::catalog::{Catalog, CatalogError};
use ferro_db::config::PAGE_SIZE;
use ferro_db::storage::buffer_pool::BufferPoolManager;
use ferro_db::table::{Column, Schema};
use ferro_db::types::{DataType, Value};

const DATABASE_FILE: &str = "data.db";
const ACCOUNTS: &str = "accounts";

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

    create_accounts_if_missing(&mut catalog, &mut bpm)?;
    let accounts = catalog.table_heap(&mut bpm, ACCOUNTS)?;
    let next_id = accounts.scan(&mut bpm).count() + 1;
    accounts.insert(&mut bpm, &new_account(i32::try_from(next_id)?))?;

    println!("tables:");
    for table in catalog.tables() {
        let columns: Vec<String> = table.schema.columns().iter().map(describe).collect();
        println!("  {} ({})", table.name, columns.join(", "));
    }

    println!("rows of {ACCOUNTS}:");
    for entry in catalog.table_heap(&mut bpm, ACCOUNTS)?.scan(&mut bpm) {
        let (rid, row) = entry?;
        let values: Vec<String> = row.iter().map(Value::to_string).collect();
        println!("  {rid} -> ({})", values.join(", "));
    }
    Ok(())
}

fn create_accounts_if_missing(
    catalog: &mut Catalog,
    bpm: &mut BufferPoolManager,
) -> Result<(), Box<dyn Error>> {
    match catalog.table(ACCOUNTS) {
        Ok(_) => Ok(()),
        Err(CatalogError::UndefinedTable { .. }) => {
            let schema = Schema::new(vec![
                Column::not_null("id", DataType::Integer),
                Column::new("age", DataType::SmallInt),
                Column::new("balance", DataType::BigInt),
            ])?;
            catalog.create_table(bpm, ACCOUNTS, schema)?;
            Ok(())
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
