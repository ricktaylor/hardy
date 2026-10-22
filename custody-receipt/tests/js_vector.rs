//! A pinned interop vector: receipts signed by the JavaScript reference
//! implementation (dtn-custody-demo, transport/bpv7/receipt-v2.mjs and
//! cose.mjs) with the seed below, generated outside this test by
//! transport/bpv7/interop/hardy-custodian/vector.mjs. Ed25519 signing is
//! deterministic, so this crate must reproduce the same bytes.

use ed25519_dalek::SigningKey;
use hardy_custody_receipt::receipt::{self, BundleIdentity, Fields};
use hex_literal::hex;

const SEED: [u8; 32] = hex!("bb1e340bfb547261639137f5bd31da905c202490eadb61ec38f2e2baaf766207");
const PAYLOAD_HASH: [u8; 32] =
    hex!("45c9f080c379add8fdc7034775b3f00ed92101bc6fc7288f9d3fb9eabb8c6ab6");
const WHOLE: &[u8] = &hex!(
    "8443a10127a05860a6016769706e3a312e30026869706e3a32302e3003826769706e3a312e31821b000000c4bdecc27b0704582045c9f080c379add8fdc7034775b3f00ed92101bc6fc7288f9d3fb9eabb8c6ab6051b000000c4bdecc5e8061b000000c4bdecc9d05840b249942b48a14bf54d689b8520f3736367dd0d2b9ef717d64c76d6ec47cbf055f622743cbd9895c818f65c43c8ef950caff90bc30634fa889ee6f167e108490f"
);
const FRAGMENT: &[u8] = &hex!(
    "8443a10127a05871a7016769706e3a312e30026869706e3a32302e3003846769706e3a312e31821b000000c4bdecc27b0719040019100004582045c9f080c379add8fdc7034775b3f00ed92101bc6fc7288f9d3fb9eabb8c6ab6051b000000c4bdecc5e8061b000000c4bdecc9d007697375626a6563742d315840ab59aebfc64bcc2f065e785d62c666af5396a8a95ec52330fd345b30d8bf0eedd2dde2c8169336b1d1119882a5fee985fcd81a41596a64d5b66ec6838bef330f"
);

fn fields(fragment: Option<(u64, u64)>, subject: Option<&str>) -> Fields<'_> {
    Fields {
        prev_hop: "ipn:1.0",
        this_hop: "ipn:20.0",
        bundle: BundleIdentity {
            source: "ipn:1.1",
            creation_time: 845_000_000_123,
            sequence_number: 7,
            fragment,
        },
        payload_hash: PAYLOAD_HASH,
        received_at: 845_000_001_000,
        forwarded_at: 845_000_002_000,
        subject,
        hop_count: None,
        chain: None,
    }
}

#[test]
fn whole_bundle_receipt_matches_the_js_reference() {
    let key = SigningKey::from_bytes(&SEED);
    assert_eq!(receipt::sign(&key, &fields(None, None).encode()), WHOLE);
}

#[test]
fn fragment_receipt_with_subject_matches_the_js_reference() {
    let key = SigningKey::from_bytes(&SEED);
    let signed = receipt::sign(
        &key,
        &fields(Some((1024, 4096)), Some("subject-1")).encode(),
    );
    assert_eq!(signed, FRAGMENT);
}

#[test]
fn js_receipts_verify_here() {
    let key = SigningKey::from_bytes(&SEED).verifying_key();
    assert_eq!(
        receipt::verify(&key, WHOLE),
        Ok(&fields(None, None).encode()[..])
    );
    let block = receipt::append(&receipt::new_block(WHOLE), FRAGMENT).unwrap();
    assert_eq!(receipt::items(&block), Ok(vec![WHOLE, FRAGMENT]));
}

// The same receipt with keys 8 (hop count 3 received, 4 released) and 9 (digest of an
// empty block, SHA-256 of 0x80), from receipt-v2.mjs (`stampBundleV2` with `chain` and `hops`).
const CHAIN_AND_HOPS: &[u8] = &hex!(
    "8443a10127a05887a8016769706e3a312e30026869706e3a32302e3003826769706e3a312e31821b000000c4bdecc27b0704582045c9f080c379add8fdc7034775b3f00ed92101bc6fc7288f9d3fb9eabb8c6ab6051b000000c4bdecc5e8061b000000c4bdecc9d00882030409582076be8b528d0075f7aae98d6fa57a6d3c83ae480a8469e668d7b0af968995ac71584050cc4f1533ba7e2d354a0e93f6a32c0bd5d3338fb63abc9ed9b4bf0e1d184c8f9b7d7b2faf2e31dc2dbd5254499dbd5438618cb09021912861321ae1b7acb80f"
);

#[test]
fn receipt_with_hop_count_and_chain_matches_the_js_reference() {
    let key = SigningKey::from_bytes(&SEED);
    let fields = Fields {
        hop_count: Some((3, 4)),
        chain: Some(hex!(
            "76be8b528d0075f7aae98d6fa57a6d3c83ae480a8469e668d7b0af968995ac71"
        )),
        ..fields(None, None)
    };
    assert_eq!(receipt::sign(&key, &fields.encode()), CHAIN_AND_HOPS);
}
