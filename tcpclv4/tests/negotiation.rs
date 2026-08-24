//! RFC 9174 contact-timeout and keepalive negotiation invariants.

use hardy_tcpclv4::{ContactTimeout, KeepaliveInterval};

// RFC 9174 Section 4.2: 1 through 60 seconds inclusive; zero and
// beyond-maximum timeouts are unrepresentable.
#[test]
fn contact_timeout_bounds() {
    assert!(ContactTimeout::new(0).is_none());
    assert_eq!(ContactTimeout::new(1).map(ContactTimeout::get), Some(1));
    assert_eq!(ContactTimeout::new(60).map(ContactTimeout::get), Some(60));
    assert!(ContactTimeout::new(61).is_none());
}

// RFC 9174 Section 4.7: the negotiated keepalive is the minimum of
// the two proposals; disabled from either side wins.
#[test]
fn keepalive_negotiation_is_a_minimum_where_disabled_wins() {
    assert_eq!(KeepaliveInterval::new(30).negotiate(60).get(), 30);
    assert_eq!(KeepaliveInterval::new(60).negotiate(30).get(), 30);
    assert_eq!(KeepaliveInterval::new(45).negotiate(45).get(), 45);
    assert!(KeepaliveInterval::DISABLED.negotiate(60).is_disabled());
    assert!(KeepaliveInterval::new(60).negotiate(0).is_disabled());
}
