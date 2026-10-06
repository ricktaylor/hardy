//! The session token.

#[cfg(feature = "server")]
use core::fmt::Write;
use core::{
    fmt,
    hash::{Hash, Hasher},
};

use hardy_bpa::Bytes;
#[cfg(feature = "server")]
use rand::{
    TryRng,
    rngs::{SysError, SysRng},
};
use subtle::ConstantTimeEq;

/// The most bytes of the subject a token keeps, so that a client-chosen name
/// cannot size the token.
#[cfg(feature = "server")]
const MAX_SUB_LEN: usize = 64;

/// The random bytes a minted token ends with, written as twice as many
/// hexadecimal digits.
#[cfg(feature = "server")]
const RANDOM_LEN: usize = 16;

/// The most bytes a minted token is: a subject cut to [`MAX_SUB_LEN`], the
/// separator, and the random suffix.
#[cfg(feature = "server")]
const MAX_TOKEN_LEN: usize = MAX_SUB_LEN + 1 + 2 * RANDOM_LEN;

/// A session token: the bearer credential a `Registration` event carries and
/// every other RPC of the session presents.
///
/// Possession is the whole proof. The server keeps the token as the key of its
/// session index, and a call presenting a token the index does not hold fails
/// with `UNAUTHENTICATED`. To a client the token is opaque bytes.
///
/// The token is not key material: it lives in plain memory for the life of
/// its session, as it does in every message that carries it. `Debug` prints
/// the token's length and never its bytes, and two tokens of the same length
/// compare in constant time.
#[derive(Clone)]
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
    /// # Errors
    ///
    /// Returns the operating system's error if its random source fails, so
    /// that one registration fails rather than the process.
    #[cfg(feature = "server")]
    pub fn mint(sub: &str) -> Result<Self, SysError> {
        let mut random = [0u8; RANDOM_LEN];
        SysRng.try_fill_bytes(&mut random)?;

        let mut end = MAX_SUB_LEN.min(sub.len());
        while !sub.is_char_boundary(end) {
            end -= 1;
        }
        let sub = &sub[..end];

        let mut token = String::with_capacity(sub.len() + 1 + 2 * RANDOM_LEN);
        token.push_str(sub);
        token.push('.');
        for b in random {
            write!(token, "{b:02x}").expect("writing to a String cannot fail");
        }
        Ok(Self(Bytes::from(token)))
    }

    /// Returns the token a call presented, or `None` if it is longer than one
    /// this server mints.
    ///
    /// The length is all that is checked, and it is checked before the session
    /// index hashes the bytes, so an unauthenticated caller cannot make the
    /// server hash a message-sized token.
    #[cfg(feature = "server")]
    pub fn presented(bytes: Bytes) -> Option<Self> {
        (bytes.len() <= MAX_TOKEN_LEN).then_some(Self(bytes))
    }
}

/// Compares the bytes in constant time, so that what a guess has in common
/// with a live token does not show in how long the comparison took.
impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for Token {}

/// Hashes the bytes, so that tokens equal under [`PartialEq`] hash alike.
impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// Prints the token's length in bytes, never its contents.
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "token({} bytes)", self.0.len())
    }
}

/// The client side holds whatever bytes the server gave it; a token a server
/// is presented with goes through [`Token::presented`] instead.
#[cfg(feature = "client")]
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

    fn mint(sub: &str) -> Token {
        Token::mint(sub).expect("the system random source is available")
    }

    #[test]
    fn the_cleartext_prefix_cannot_size_the_token() {
        let token = Bytes::from(mint(&"a".repeat(64 * 1024)));
        assert!(
            token.len() <= MAX_TOKEN_LEN,
            "a {} byte token was minted",
            token.len()
        );
    }

    #[test]
    fn truncating_the_prefix_keeps_the_token_printable() {
        let token = Bytes::from(mint(&"é".repeat(MAX_SUB_LEN)));
        from_utf8(&token).expect("a token is printable UTF-8");
    }

    #[test]
    fn a_token_longer_than_a_minted_one_is_not_presentable() {
        let minted = Bytes::from(mint(&"a".repeat(MAX_SUB_LEN)));
        assert_eq!(minted.len(), MAX_TOKEN_LEN);

        assert!(Token::presented(minted).is_some());
        assert!(Token::presented(Bytes::from(vec![b'a'; MAX_TOKEN_LEN])).is_some());
        assert!(Token::presented(Bytes::from(vec![b'a'; MAX_TOKEN_LEN + 1])).is_none());
    }

    #[test]
    fn a_token_is_only_equal_to_itself() {
        let token = mint("ipn:1.7");
        let other = mint("ipn:1.7");

        assert_eq!(token, token.clone());
        assert_ne!(
            token, other,
            "two registrations under one identity got the same token"
        );
        let prefix = Bytes::from(token.clone()).slice(..8);
        assert_ne!(token, Token::presented(prefix).unwrap());
    }
}
