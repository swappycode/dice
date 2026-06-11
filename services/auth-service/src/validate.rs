//! Pure input validation for registration (docs/protocol.md §10, §12).
//!
//! All failures map to [`AuthError::InvalidArgument`] so the gateway can
//! return one consistent error shape for malformed bodies.

use crate::AuthError;

/// Minimum username length (bytes — the charset is ASCII-only).
pub const USERNAME_MIN: usize = 2;
/// Maximum username length.
pub const USERNAME_MAX: usize = 32;
/// Minimum password length in characters.
pub const PASSWORD_MIN: usize = 8;
/// Maximum password length in characters.
pub const PASSWORD_MAX: usize = 128;
/// RFC 5321 total-length ceiling; we only enforce basic shape beyond this.
const EMAIL_MAX: usize = 254;

/// Basic email shape: `local@domain` with a dotted, non-degenerate domain.
/// Deliverability is NOT verified in M1 (no verification mail flow).
pub fn email(email: &str) -> Result<(), AuthError> {
    if is_valid_email(email) {
        Ok(())
    } else {
        Err(AuthError::InvalidArgument(
            "malformed email address".to_owned(),
        ))
    }
}

fn is_valid_email(e: &str) -> bool {
    if e.len() > EMAIL_MAX || e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    // rsplit: an unquoted local part cannot contain '@', so the LAST '@'
    // separates local from domain.
    let Some((local, domain)) = e.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

/// Username must match `^[a-z0-9_.]{2,32}$` (mirrors the `users.username`
/// CHECK constraint, so a DB constraint error here is always a server bug).
pub fn username(u: &str) -> Result<(), AuthError> {
    let ok = (USERNAME_MIN..=USERNAME_MAX).contains(&u.len())
        && u.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(AuthError::InvalidArgument(
            "username must match ^[a-z0-9_.]{2,32}$".to_owned(),
        ))
    }
}

/// Password length 8..=128 characters (chars, not bytes — multi-byte
/// passphrases count by what the user typed).
pub fn password(p: &str) -> Result<(), AuthError> {
    let n = p.chars().count();
    if (PASSWORD_MIN..=PASSWORD_MAX).contains(&n) {
        Ok(())
    } else {
        Err(AuthError::InvalidArgument(
            "password must be 8..=128 characters".to_owned(),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn is_invalid_argument(r: Result<(), AuthError>) -> bool {
        matches!(r, Err(AuthError::InvalidArgument(_)))
    }

    #[test]
    fn email_accepts_basic_shapes() {
        for ok in [
            "a@b.co",
            "user.name+tag@example.com",
            "x@sub.domain.example",
            "UPPER@EXAMPLE.COM",
        ] {
            assert!(email(ok).is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn email_rejects_malformed() {
        let long = format!("{}@example.com", "a".repeat(250));
        for bad in [
            "",
            "plain",
            "@example.com",
            "a@",
            "a@nodot",
            "a@.com",
            "a@com.",
            "a@do..com",
            "has space@example.com",
            "tab\t@example.com",
            long.as_str(),
        ] {
            assert!(
                is_invalid_argument(email(bad)),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn username_charset_and_length() {
        for ok in ["ab", "a1", "user_name.42", &"a".repeat(32)] {
            assert!(username(ok).is_ok(), "{ok} should be accepted");
        }
        for bad in [
            "",
            "a",
            "UPPER",
            "has-dash",
            "has space",
            "emoji🦀",
            &"a".repeat(33),
        ] {
            assert!(
                is_invalid_argument(username(bad)),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn password_length_in_chars() {
        assert!(is_invalid_argument(password("1234567")));
        assert!(password("12345678").is_ok());
        assert!(password(&"p".repeat(128)).is_ok());
        assert!(is_invalid_argument(password(&"p".repeat(129))));
        // 8 multi-byte chars = 8 chars even though > 8 bytes.
        assert!(password("ßßßßßßßß").is_ok());
    }
}
