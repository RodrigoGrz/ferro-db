use std::fmt;

use crate::storage::buffer_pool::BufferPoolError;
use crate::types::{DataType, Value};

#[derive(Debug)]
pub enum IndexError {
    /// The key is already in the tree; nothing changed.
    DuplicateKey {
        key: String,
    },
    /// The encoded key is longer than a node allows (`BPlusTree::max_key_size`).
    KeyTooLarge {
        size: usize,
        max: usize,
    },
    /// The key must already be of the tree's key type; converting is the caller's job.
    KeyTypeMismatch {
        expected: DataType,
        found: DataType,
    },
    /// `NULL` is never a key: the index of a table skips rows whose key is `NULL`.
    NullKey,
    /// The buffer pool's page size cannot hold four keys of the largest fixed type.
    UnsupportedPageSize {
        size: usize,
    },
    /// A page of the tree that cannot be what it should: never trusted, never a panic.
    Corrupted {
        page_id: u32,
        reason: &'static str,
    },
    BufferPool(BufferPoolError),
}

impl IndexError {
    /// `DuplicateKey` for `key`, shown cut at 64 characters: a 2 KB key does not make a 2 KB
    /// message. For callers that find a repeated key before asking the tree.
    pub fn duplicate_key(key: &Value) -> IndexError {
        const MAX_SHOWN_CHARS: usize = 64;
        let text = key.to_string();
        let key = match text.char_indices().nth(MAX_SHOWN_CHARS) {
            Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()),
            None => text,
        };
        IndexError::DuplicateKey { key }
    }

    pub fn sqlstate(&self) -> &'static str {
        match self {
            IndexError::DuplicateKey { .. } => "23505",
            IndexError::KeyTooLarge { .. } => "54000",
            IndexError::KeyTypeMismatch { .. } => "42804",
            IndexError::NullKey | IndexError::UnsupportedPageSize { .. } => "XX000",
            IndexError::Corrupted { .. } => "XX001",
            IndexError::BufferPool(err) => err.sqlstate(),
        }
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::DuplicateKey { key } => {
                write!(f, "duplicate key value ({key}) violates the unique index")
            }
            IndexError::KeyTooLarge { size, max } => write!(
                f,
                "index key of {size} bytes exceeds the maximum of {max} bytes"
            ),
            IndexError::KeyTypeMismatch { expected, found } => write!(
                f,
                "an index key of type {} does not match the index type {}",
                found.name().to_ascii_lowercase(),
                expected.name().to_ascii_lowercase()
            ),
            IndexError::NullKey => write!(f, "NULL cannot be an index key"),
            IndexError::UnsupportedPageSize { size } => {
                write!(f, "page size {size} is too small for a B+Tree")
            }
            IndexError::Corrupted { page_id, reason } => {
                write!(f, "B+Tree page {page_id} is corrupted: {reason}")
            }
            IndexError::BufferPool(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IndexError::BufferPool(err) => Some(err),
            _ => None,
        }
    }
}

impl From<BufferPoolError> for IndexError {
    fn from(err: BufferPoolError) -> Self {
        IndexError::BufferPool(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;

    fn every_error() -> Vec<(IndexError, &'static str, &'static str, bool)> {
        vec![
            (
                IndexError::DuplicateKey {
                    key: "42".to_string(),
                },
                "duplicate key value (42) violates the unique index",
                "23505",
                false,
            ),
            (
                IndexError::KeyTooLarge {
                    size: 3000,
                    max: 2031,
                },
                "index key of 3000 bytes exceeds the maximum of 2031 bytes",
                "54000",
                false,
            ),
            (
                IndexError::KeyTypeMismatch {
                    expected: DataType::Varchar(NonZeroU16::new(5).unwrap()),
                    found: DataType::Integer,
                },
                "an index key of type integer does not match the index type varchar",
                "42804",
                false,
            ),
            (
                IndexError::NullKey,
                "NULL cannot be an index key",
                "XX000",
                false,
            ),
            (
                IndexError::UnsupportedPageSize { size: 64 },
                "page size 64 is too small for a B+Tree",
                "XX000",
                false,
            ),
            (
                IndexError::Corrupted {
                    page_id: 7,
                    reason: "keys are not in increasing order",
                },
                "B+Tree page 7 is corrupted: keys are not in increasing order",
                "XX001",
                false,
            ),
            (
                IndexError::BufferPool(BufferPoolError::ChecksumMismatch { page_id: 7 }),
                "checksum mismatch on page 7",
                "XX001",
                true,
            ),
            (
                IndexError::BufferPool(BufferPoolError::NoAvailableFrame),
                "no available frame",
                "53000",
                true,
            ),
        ]
    }

    #[test]
    fn duplicate_key_should_show_a_long_key_cut() {
        let short = IndexError::duplicate_key(&Value::Integer(7));
        let long = IndexError::duplicate_key(&Value::Text("x".repeat(100)));

        assert!(matches!(&short, IndexError::DuplicateKey { key } if key == "7"));
        let shown = format!("{}…", "x".repeat(64));
        assert!(
            matches!(&long, IndexError::DuplicateKey { key } if *key == shown),
            "{long:?}"
        );
    }

    #[test]
    fn errors_should_describe_the_problem() {
        for (err, message, _, _) in every_error() {
            assert_eq!(err.to_string(), message);
        }
    }

    #[test]
    fn errors_should_map_to_sqlstates() {
        for (err, _, sqlstate, _) in every_error() {
            assert_eq!(err.sqlstate(), sqlstate, "{err:?}");
        }
    }

    #[test]
    fn wrapped_errors_should_expose_their_source() {
        for (err, _, _, has_source) in every_error() {
            assert_eq!(
                std::error::Error::source(&err).is_some(),
                has_source,
                "{err:?}"
            );
        }
    }
}
