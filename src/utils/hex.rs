use std::fmt::Write;

/// Lowercase hex encoding of a byte slice.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_bytes_as_lowercase_hex() {
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00, 0xff, 0x0a]), "00ff0a");
    }
}
