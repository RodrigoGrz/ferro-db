mod boolean;
pub mod data_type;
pub mod error;
mod float;
mod integer;
mod text;
pub mod value;

pub use data_type::DataType;
pub use error::TypeError;
pub use float::{Finite32, Finite64};
pub use value::Value;
