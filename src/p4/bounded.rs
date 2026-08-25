use std::io::{self, Read};

/// Reads `reader` until EOF, failing if the byte budget would be exceeded.
///
/// The limit is inclusive: `limit` bytes succeed, `limit + 1` fails without
/// retaining the overflow in memory.
pub fn read_limited<R: Read>(mut reader: R, limit: usize) -> Result<Vec<u8>, BoundedReadError> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(BoundedReadError::Io(error)),
        };
        if buffer.len().saturating_add(read) > limit {
            return Err(BoundedReadError::LimitExceeded);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(buffer)
}

#[derive(Debug)]
pub enum BoundedReadError {
    LimitExceeded,
    Io(io::Error),
}

impl std::fmt::Display for BoundedReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LimitExceeded => {
                formatter.write_str("output exceeded the configured byte budget")
            }
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl PartialEq for BoundedReadError {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::LimitExceeded, Self::LimitExceeded) | (Self::Io(_), Self::Io(_))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn empty_read_is_within_zero_budget() {
        let bytes = read_limited(Cursor::new([]), 0).expect("empty output fits");
        assert!(bytes.is_empty());
    }

    #[test]
    fn exact_limit_is_accepted() {
        let bytes = read_limited(Cursor::new(b"abcd"), 4).expect("exact budget fits");
        assert_eq!(bytes, b"abcd");
    }

    #[test]
    fn overflowing_budget_fails_without_returning_bytes() {
        let error = read_limited(Cursor::new(b"abcde"), 4).expect_err("over budget");
        assert_eq!(error, BoundedReadError::LimitExceeded);
    }
}
