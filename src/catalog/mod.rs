#[allow(clippy::module_inception)]
pub mod catalog;
pub mod error;
pub mod header;
pub mod records;

pub use catalog::{Catalog, IndexId, IndexInfo, TableId, TableInfo};
pub use error::CatalogError;
pub use header::DatabaseHeader;
