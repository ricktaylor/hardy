//! TLS configuration builder validation, through the public `tls` API.

use hardy_tcpclv4::tls::{ClientAuth, Error, Tls};

// A trust anchor is the one mandatory input, judged before any file
// IO happens.
#[test]
fn a_trust_anchor_is_mandatory() {
    let err = Tls::builder().build().unwrap_err();
    assert!(matches!(err, Error::NoTrustAnchor));
}

// insecure_skip_verify overrides ca_certs rather than conflicting with
// it. The ca_certs path is bogus on purpose: the build succeeding
// proves the ignored certificates are never loaded.
#[test]
fn insecure_skip_verify_overrides_ca_certs() {
    Tls::builder()
        .ca_certs("/nonexistent/ca".into())
        .dangerous()
        .insecure_skip_verify()
        .build()
        .expect("insecure-skip-verify wins; ca-certs is not loaded");
}

// Client verification without an identity is rejected before any file
// IO happens.
#[test]
fn client_auth_requires_identity() {
    let err = Tls::builder()
        .dangerous()
        .insecure_skip_verify()
        .client_auth(ClientAuth::Required)
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::ClientAuthWithoutIdentity));
}

// Client verification against insecure trust has no anchors to verify
// with and is rejected before any file IO happens.
#[test]
fn client_auth_requires_anchors() {
    let err = Tls::builder()
        .dangerous()
        .insecure_skip_verify()
        .identity("cert.pem".into(), "key.pem".into())
        .client_auth(ClientAuth::Optional)
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::ClientAuthWithoutAnchors));
}
