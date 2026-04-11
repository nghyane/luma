//! Cross-cutting helpers shared by more than one module.

/// Generate a UUIDv4 canonical string (RFC 4122) using OS entropy.
///
/// Returns `None` if the OS entropy source is unavailable; callers should
/// degrade gracefully rather than panic.
pub fn uuid_v4() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-\
         {:02x}{:02x}-\
         {:02x}{:02x}-\
         {:02x}{:02x}-\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    ))
}

/// Loose UUID canonical-form check (8-4-4-4-12 hex, case-insensitive).
pub fn is_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let dashes = [8, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        if dashes.contains(&i) {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_is_well_formed() {
        let u = uuid_v4().expect("entropy");
        assert!(is_uuid(&u));
        // Version nibble at hex position 14 must be '4'.
        assert_eq!(u.as_bytes()[14], b'4');
        // Variant nibble at hex position 19 must be 8/9/a/b.
        let v = u.as_bytes()[19];
        assert!(matches!(v, b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn uuid_v4_is_random() {
        let a = uuid_v4().unwrap();
        let b = uuid_v4().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn is_uuid_accepts_canonical() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_uuid("550E8400-E29B-41D4-A716-446655440000"));
    }

    #[test]
    fn is_uuid_rejects_bad_shape() {
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("550e8400e29b41d4a716446655440000"));
        assert!(!is_uuid("550e8400-e29b-41d4-a716-44665544000z"));
        assert!(!is_uuid(""));
    }
}
