//! The session token.

use core::fmt;
#[cfg(feature = "server")]
use core::fmt::Write;

use hardy_bpa::Bytes;
#[cfg(feature = "server")]
use rand::{TryRng, rngs::SysRng};

/// The most bytes of the subject a token keeps, so that a client-chosen name
/// cannot size the token.
#[cfg(feature = "server")]
const MAX_SUB_LEN: usize = 64;

/// A session token: the bearer credential a `Registration` event carries and
/// every other RPC of the session presents.
///
/// Possession is the whole proof. The server keeps the token as the key of its
/// session index, and a call presenting a token the index does not hold fails
/// with `UNAUTHENTICATED`. To a client the token is opaque bytes.
///
/// The token is not key material: it lives in plain memory for the life of
/// its session, as it does in every message that carries it. `Debug` prints
/// the token's length and never its bytes.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Token(Bytes);

impl Token {
    /// Mints a fresh token for the subject `sub`.
    ///
    /// The token is `sub`, cut to at most 64 bytes on a character boundary, then a
    /// `.`, then 32 hexadecimal digits drawn from the operating system's random
    /// source. The prefix lets an operator attribute a token to its session; the
    /// suffix is what makes it unguessable, and two tokens minted for the same
    /// subject differ.
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random source fails.
    #[cfg(feature = "server")]
    pub fn mint(sub: &str) -> Self {
        let mut random = [0u8; 16];
        SysRng
            .try_fill_bytes(&mut random)
            .expect("the system random source failed");

        let mut end = MAX_SUB_LEN.min(sub.len());
        while !sub.is_char_boundary(end) {
            end -= 1;
        }
        let sub = &sub[..end];

        let mut token = String::with_capacity(sub.len() + 33);
        token.push_str(sub);
        token.push('.');
        for b in random {
            write!(token, "{b:02x}").expect("writing to a String cannot fail");
        }
        Self(Bytes::from(token))
    }
}

/// Prints the token's length in bytes, never its contents.
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "token({} bytes)", self.0.len())
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
    use core::str::from_utf8;

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
        let token = Bytes::from(Token::mint(&"é".repeat(MAX_SUB_LEN)));
        from_utf8(&token).expect("a token is printable UTF-8");
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
