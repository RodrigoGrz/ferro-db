//! Text semantics of VARCHAR(n). Lengths count characters (Unicode scalar values, Rust's
//! `char`), not bytes: `'ação'` fits VARCHAR(4) though its UTF-8 takes 6 bytes.

use std::num::NonZeroU16;

use super::{DataType, TypeError, Value};

/// Splits after the first `max_chars` characters; the second part is the excess.
fn split_at_char(text: &str, max_chars: NonZeroU16) -> (&str, &str) {
    match text.char_indices().nth(usize::from(max_chars.get())) {
        Some((byte_index, _)) => text.split_at(byte_index),
        None => (text, ""),
    }
}

/// Store assignment: text longer than `n` is an error, unless the excess is only spaces
/// (U+0020), which are cut. Tabs and other blanks count as data. Returns the part to keep.
fn assigned_part(max_chars: NonZeroU16, text: &str) -> Result<&str, TypeError> {
    let (kept, excess) = split_at_char(text, max_chars);
    if excess.bytes().all(|byte| byte == b' ') {
        Ok(kept)
    } else {
        Err(TypeError::StringTooLong {
            ty: DataType::Varchar(max_chars),
            chars: text.chars().count(),
        })
    }
}

pub(crate) fn assign(max_chars: NonZeroU16, text: &str) -> Result<Value, TypeError> {
    assigned_part(max_chars, text).map(|kept| Value::Text(kept.to_string()))
}

/// Explicit CAST: the excess is cut, whatever it holds.
pub(crate) fn truncate(max_chars: NonZeroU16, text: &str) -> Value {
    let (kept, _) = split_at_char(text, max_chars);
    Value::Text(kept.to_string())
}

/// `||`: a result longer than the widest VARCHAR follows the assignment rule, so only an
/// excess of spaces is cut.
pub(crate) fn concat(lhs: &str, rhs: &str) -> Result<Value, TypeError> {
    let mut joined = [lhs, rhs].concat();
    // A prefix of `joined` that ends on a character boundary, so `truncate` cannot panic.
    let kept_len = assigned_part(NonZeroU16::MAX, &joined)?.len();
    joined.truncate(kept_len);
    Ok(Value::Text(joined))
}

/// A stored field. Writes go through `assign`, so a field that is not UTF-8 or is longer
/// than its column was damaged on disk or read with the wrong schema.
pub(crate) fn decode(max_chars: NonZeroU16, bytes: &[u8]) -> Result<Value, TypeError> {
    let corrupted = |reason| TypeError::CorruptedText {
        ty: DataType::Varchar(max_chars),
        reason,
    };
    let text = std::str::from_utf8(bytes).map_err(|_| corrupted("text is not valid UTF-8"))?;
    let (_, excess) = split_at_char(text, max_chars);
    if !excess.is_empty() {
        return Err(corrupted("text is longer than its column"));
    }
    Ok(Value::Text(text.to_string()))
}
