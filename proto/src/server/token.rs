// Session tokens: unguessable, self-describing bearer tokens for the
// RPCs of one Subscribe session.
//
// The registration handshake mints a token; every later RPC of that
// session presents it, and the bridge resolves it directly as the
// session-map key: the token embeds an unguessable random session id,
// so possession is the proof and a forged token is simply absent from
// the map. Nothing verifies more than possession, so the token is
// plain random bytes rather than anything signed; the cleartext `sub`
// prefix (the registration identity) keeps it self-describing for
// debugging. A signed shape (for evolutions such as reconnectable
// sessions, where a token must outlive the map entry) can replace the
// mint without touching any resolver.
//
// The token does not expire: the session map is the authority on
// liveness, and a token expiring mid-session would break a live
// long-running peer.

use core::fmt::Write;

use hardy_bpa::Bytes;

/// A minted session token: the bearer credential of one Subscribe
/// session, and the session-map key that resolves its RPCs.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionToken(Bytes);

impl SessionToken {
    /// A fresh token issued to the registration identity `sub` (an
    /// endpoint id or component name): the cleartext identity, a
    /// separator, and 128 random bits that make the token unguessable.
    pub fn mint(sub: &str) -> Self {
        let mut token = String::with_capacity(sub.len() + 33);
        token.push_str(sub);
        token.push('.');
        for b in rand::random::<[u8; 16]>() {
            let _ = write!(token, "{b:02x}");
        }
        Self(Bytes::from(token.into_bytes()))
    }
}

impl From<Bytes> for SessionToken {
    // A token as presented by an RPC.
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl From<SessionToken> for Bytes {
    // A token as carried on the wire.
    fn from(token: SessionToken) -> Self {
        token.0
    }
}
