/// Default size of every page, in bytes.
pub const PAGE_SIZE: usize = 8192;

/// Largest page whose offsets still fit in a `u16` (slotted page slots store `u16` offsets).
pub const MAX_PAGE_SIZE: usize = u16::MAX as usize;

const _: () = assert!(PAGE_SIZE <= MAX_PAGE_SIZE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_should_fit_u16_offsets() {
        assert!(u16::try_from(PAGE_SIZE).is_ok());
        assert!(u16::try_from(MAX_PAGE_SIZE).is_ok());
    }
}
