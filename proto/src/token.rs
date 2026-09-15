// Session tokens: unguessable bearer tokens for the RPCs of one
// Subscribe session. One type for both ends of the wire, so neither can
// handle a credential as plain bytes.
//
// Registration mints a token, and the server resolves it as the
// session-map key: possession is the proof, and a forged token is
// simply absent from the map. Nothing verifies more than that, so the
// token is plain random bytes; the cleartext `sub` prefix keeps it
// self-describing for debugging. A signed shape can replace the mint
// without touching any resolver.
//
// The token does not expire: the session map is the authority on
// liveness, and expiry would break a long-running peer mid-session.

#[cfg(feature = "server")]
use core::fmt::Write;

use hardy_bpa::Bytes;
#[cfg(feature = "server")]
use rand::{TryRng, rngs::SysRng};
#[cfg(feature = "server")]
use zeroize::Zeroizing;

/// How much of the registration identity a token carries. The prefix
/// is client-supplied on the CLA and routing surfaces (a component
/// names itself at registration) and nothing upstream bounds it, while
/// the token it sizes is held for the whole session and echoed on every
/// data-plane RPC in both directions: unbounded, a 16 MiB registration
/// name would mint a 16 MiB bearer token. Past this the prefix is
/// truncated, which costs nothing that matters: it is there to make a
/// token recognisable in a log, and the random suffix alone is what
/// makes one unguessable and distinct.
#[cfg(feature = "server")]
const MAX_SUB_LEN: usize = 64;

/// The bearer credential of one Subscribe session: the server's
/// session-map key, and the thing every token-gated call of that
/// session carries in its first message.
///
/// The wire type is `bytes`, but a bare [`Bytes`] is a credential that
/// prints itself: it reaches a log the moment any struct holding it is
/// `Debug`-formatted. This type deliberately has no `Debug` and no
/// `Display`, so rendering a token is a compile error rather than a
/// redaction someone has to remember to write.
///
/// The bytes are not zeroized on drop. [`Bytes`] is refcounted and
/// immutable, so there is no sound way to wipe them: a clone rides in
/// every message prost encodes, and each buffer one side wiped would
/// still leave those behind. A token's secrecy rests on its lifetime
/// instead: the session map stops resolving it the moment the session
/// ends.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Token(Bytes);

impl Token {
    /// A fresh token issued to the registration identity `sub` (an
    /// endpoint id or component name): the cleartext identity, a
    /// separator, and 128 random bits that make the token unguessable.
    /// The identity is truncated to [`MAX_SUB_LEN`] bytes.
    ///
    /// # Panics
    ///
    /// If the system random source fails, which is not a condition a
    /// session can be opened under: a token the OS could not make
    /// unguessable would be a credential in name only.
    #[cfg(feature = "server")]
    pub fn mint(sub: &str) -> Self {
        // Only the binary form is wiped: the hex rendering *is* the
        // token, and lives as long as the session does.
        let mut random = Zeroizing::new([0u8; 16]);
        SysRng
            .try_fill_bytes(random.as_mut_slice())
            .expect("the system random source failed");

        // On a char boundary, so the prefix stays printable UTF-8.
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

    /// The token as a request carries it. [`Bytes`] is refcounted, so
    /// this shares the buffer rather than copying the credential.
    #[cfg(feature = "client")]
    pub fn to_bytes(&self) -> Bytes {
        self.0.clone()
    }
}

impl From<Bytes> for Token {
    // A token as an RPC presented it, or as a Registration delivered it.
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl From<Token> for Bytes {
    // A token as carried on the wire.
    fn from(token: Token) -> Self {
        token.0
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn the_cleartext_prefix_cannot_size_the_token() {
        // A registration name is client-supplied and unbounded; the
        // token it mints is not.
        let token = Bytes::from(Token::mint(&"a".repeat(64 * 1024)));
        assert!(
            token.len() <= MAX_SUB_LEN + 33,
            "a {} byte token was minted",
            token.len()
        );
    }

    #[test]
    fn truncating_the_prefix_keeps_the_token_printable() {
        // Two bytes per char, so the bound falls mid-character unless
        // the truncation looks for a boundary.
        let token = Bytes::from(Token::mint(&"é".repeat(MAX_SUB_LEN)));
        core::str::from_utf8(&token).expect("a token is printable UTF-8");
    }

    #[test]
    fn identities_do_not_determine_the_token() {
        // Two registrations under one identity must still get distinct
        // tokens: the prefix carries no authority, the random suffix is
        // the whole credential. Compared without printing either, since
        // a failure has no business logging a credential.
        let first = Bytes::from(Token::mint("ipn:1.7"));
        let second = Bytes::from(Token::mint("ipn:1.7"));
        assert!(
            first != second,
            "two registrations under one identity got the same token"
        );
    }
}
