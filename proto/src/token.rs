// Bearer tokens authenticating the RPCs of one Subscribe session. The
// server uses the token as its session-map key; tokens do not expire.

#[cfg(feature = "server")]
use core::fmt::Write;

use hardy_bpa::Bytes;
#[cfg(feature = "server")]
use rand::{TryRng, rngs::SysRng};
#[cfg(feature = "server")]
use zeroize::Zeroizing;

// Caps the client-supplied prefix, so a peer cannot inflate the token.
#[cfg(feature = "server")]
const MAX_SUB_LEN: usize = 64;

// The bearer credential of one Subscribe session. It has no `Debug` or
// `Display` impl, so a token cannot be formatted into logs or errors.
// Not zeroized: `Bytes` is refcounted and immutable, so the buffer
// cannot be safely wiped.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Token(Bytes);

impl Token {
    // Mints `sub` truncated to `MAX_SUB_LEN`, a `.`, then 128 random
    // bits in hex. Panics if the system random source fails.
    #[cfg(feature = "server")]
    pub fn mint(sub: &str) -> Self {
        let mut random = Zeroizing::new([0u8; 16]);
        SysRng
            .try_fill_bytes(random.as_mut_slice())
            .expect("the system random source failed");

        // Truncate on a char boundary so the prefix stays valid UTF-8.
        let mut end = MAX_SUB_LEN.min(sub.len());
        while !sub.is_char_boundary(end) {
            end -= 1;
        }
        let sub = &sub[..end];

        let mut token = String::with_capacity(sub.len() + 33);
        token.push_str(sub);
        token.push('.');
        for b in random.iter() {
            let _ = write!(token, "{b:02x}");
        }
        Self(Bytes::from(token.into_bytes()))
    }

    #[cfg(feature = "client")]
    pub fn to_bytes(&self) -> Bytes {
        self.0.clone()
    }
}

impl From<Bytes> for Token {
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl From<Token> for Bytes {
    fn from(token: Token) -> Self {
        token.0
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn the_cleartext_prefix_cannot_size_the_token() {
        let token = Bytes::from(Token::mint(&"a".repeat(64 * 1024)));
        assert!(
            token.len() <= MAX_SUB_LEN + 33,
            "a {} byte token was minted",
            token.len()
        );
    }

    #[test]
    fn truncating_the_prefix_keeps_the_token_printable() {
        // Two-byte chars, so the bound falls mid-character.
        let token = Bytes::from(Token::mint(&"é".repeat(MAX_SUB_LEN)));
        core::str::from_utf8(&token).expect("a token is printable UTF-8");
    }

    #[test]
    fn identities_do_not_determine_the_token() {
        let first = Bytes::from(Token::mint("ipn:1.7"));
        let second = Bytes::from(Token::mint("ipn:1.7"));
        assert!(
            first != second,
            "two registrations under one identity got the same token"
        );
    }
}
