// Session tokens: signed, self-describing bearer tokens for the RPCs
// of one Subscribe session.
//
// One `Signer` with a process-local secret is created where the gRPC
// routes are composed and shared by every surface: the signing
// identity belongs to the server, not to a surface. The registration
// handshake mints a token; every later RPC of that session presents
// it, and the bridge resolves it directly as the session-map key: the
// token embeds an unguessable random session id, so possession is the
// proof and a forged token is simply absent from the map. The JWT
// shape keeps the token self-describing (the `sub` claim carries the
// registration identity) for debugging and for later evolutions such
// as reconnectable sessions.
//
// The token does not expire: the session map is the authority on
// liveness, and a token expiring mid-session would break a live
// long-running peer.

use core::fmt::Write;

use hardy_bpa::Bytes;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;

/// A minted session token: the bearer credential of one Subscribe
/// session, and the session-map key that resolves its RPCs.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionToken(Bytes);

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

// A session token's claims: the registration identity it belongs to,
// and the random session id that makes the token unguessable.
#[derive(Serialize)]
struct Claims {
    sub: String,
    sid: String,
}

/// Mints session tokens with a process-local secret.
#[derive(Clone)]
pub struct Signer {
    encoding: EncodingKey,
}

impl Default for Signer {
    fn default() -> Self {
        Self::new()
    }
}

impl Signer {
    /// A signer with a fresh random secret.
    pub fn new() -> Self {
        let secret: [u8; 32] = rand::random();
        Self {
            encoding: EncodingKey::from_secret(&secret),
        }
    }

    /// A fresh session token issued to the registration identity `sub`
    /// (an endpoint id or component name).
    pub fn mint(&self, sub: &str) -> SessionToken {
        let mut sid = String::with_capacity(32);
        for b in rand::random::<[u8; 16]>() {
            let _ = write!(sid, "{b:02x}");
        }
        let claims = Claims {
            sub: sub.to_string(),
            sid,
        };
        // Signing with an in-memory HS256 key cannot fail for this
        // claim shape; a failure is a bug, not a runtime condition.
        let token = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .expect("session token encoding");
        SessionToken(Bytes::from(token.into_bytes()))
    }
}
