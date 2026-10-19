//! The custodian through a real BPA: bundles arrive from a peer, cross the
//! Egress Rewriter chain, and the bytes the CLA is handed are checked. Covers
//! a whole bundle, fragments, a BCB-covered payload (left unsigned), and the
//! `block-present` policy.

use core::num::NonZeroU64;
use std::sync::Arc;

use ed25519_dalek::{SigningKey, VerifyingKey};
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla,
    node_ids::NodeIds,
    stream::{Receiver, buffer_stream},
};
use hardy_bpv7::{
    block::{Block, Flags, Type},
    bpsec::{
        encryptor::{Context, Encryptor},
        key::{EncAlgorithm, Key, Operation, Type as KeyType},
    },
    builder::Builder,
    bundle::{Bundle, FragmentInfo, Id},
    crc::CrcType,
    creation_timestamp::CreationTimestamp,
    editor::{Chunk, Editor},
    eid::{IpnNodeId, NodeId},
    parse::{Parsed, parse},
};
use hardy_custody_receipt::{
    CustodyReceipt, Policy,
    receipt::{self, BLOCK_TYPE, BundleIdentity, Fields},
};
use rand::{TryRng, rngs::SysRng};
use sha2::{Digest, Sha256};

// A remote source and a remote destination behind the one peer, ipn:0.2.
const SOURCE: &str = "ipn:0.3.1";
const REMOTE: &str = "ipn:0.2.99";
const THIS_HOP: &str = "ipn:1.0";
const PEER: &str = "ipn:2.0";

struct CapturingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    forwarded_tx: flume::Sender<Bytes>,
}

#[async_trait]
impl cla::Cla for CapturingCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let bundle = buffer_stream(stream, total_len).await?;
        let _ = self.forwarded_tx.send(bundle);
        Ok(cla::ForwardBundleResult::Sent)
    }
}

fn node(node_number: u32) -> NodeId {
    NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number,
    })
}

// Immaterial key: generated per the no-literal-keys rule.
fn signing_key() -> SigningKey {
    let mut seed = [0u8; 32];
    SysRng.try_fill_bytes(&mut seed).unwrap();
    SigningKey::from_bytes(&seed)
}

// A BPA as ipn:0.1 running the custodian, with one peer serving ipn:0.2.
async fn setup(policy: Policy) -> (Bpa, Arc<CapturingCla>, flume::Receiver<Bytes>, VerifyingKey) {
    let custodian = CustodyReceipt::new(THIS_HOP.into(), signing_key(), policy);
    let key = custodian.verifying_key();
    let node_ids = NodeIds::try_from([node(1)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .add_filters(custodian.filter_pack())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (forwarded_tx, forwarded_rx) = flume::bounded(16);
    let cla = Arc::new(CapturingCla {
        sink: hardy_async::sync::spin::Once::new(),
        forwarded_tx,
    });
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[node(2)],
        )
        .await
        .unwrap();
    (bpa, cla, forwarded_rx, key)
}

async fn arrive(cla: &CapturingCla, data: Box<[u8]>) {
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&node(2)), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
}

async fn next(rx: &flume::Receiver<Bytes>) -> Parsed {
    // Event-driven wait; the timeout only bounds a regression.
    let bytes = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv_async())
        .await
        .expect("Timeout waiting for the pipeline")
        .expect("Channel closed");
    parse(bytes).expect("the forwarded bundle must parse")
}

fn plain(payload: &[u8]) -> (Bundle, Box<[u8]>) {
    Builder::new(SOURCE.parse().unwrap(), REMOTE.parse().unwrap())
        .with_payload(payload.into())
        .build(CreationTimestamp::now())
        .unwrap()
}

// The same bundle as a fragment: `payload` is the fragment's slice of an
// ADU `total` bytes long, starting at `offset`.
fn fragment(payload: &[u8], offset: u64, total: u64) -> Box<[u8]> {
    let (bundle, data) = plain(payload);
    let chunks = Editor::new(&bundle, &data)
        .with_fragment_info(Some(FragmentInfo {
            offset,
            total_adu_length: total,
        }))
        .unwrap_or_else(|(_, e)| panic!("{e}"))
        .rebuild()
        .unwrap();
    Chunk::flatten(chunks, &data)
}

fn custody_block(parsed: &Parsed) -> Option<(&Block, &[u8])> {
    let mut blocks = parsed
        .bundle
        .blocks
        .values()
        .filter(|b| b.block_type == Type::Unrecognised(BLOCK_TYPE));
    let block = blocks.next()?;
    assert!(blocks.next().is_none(), "at most one Custody Receipt Block");
    Some((block, block.payload(&parsed.data).unwrap()))
}

// The receipt payload up to the payload hash (keys 1 to 4), which the test
// knows in full; the times (keys 5 and 6) are the node's to choose. The
// encoder ends a two-time map with `05 00 06 00` when both are zero.
fn known_prefix(prev_hop: &str, bundle: BundleIdentity<'_>, payload: &[u8]) -> Vec<u8> {
    let mut encoded = Fields {
        prev_hop,
        this_hop: THIS_HOP,
        bundle,
        payload_hash: Sha256::digest(payload).into(),
        received_at: 0,
        forwarded_at: 0,
        subject: None,
    }
    .encode();
    assert_eq!(
        encoded.split_off(encoded.len() - 4),
        [0x05, 0x00, 0x06, 0x00]
    );
    encoded
}

fn identity<'a>(parsed: &Parsed, source: &'a str) -> BundleIdentity<'a> {
    let id = &parsed.bundle.primary.id;
    let (time, sequence_number) = id.timestamp.clone().into_parts();
    BundleIdentity {
        source,
        creation_time: time.map_or(0, |t| t.millisecs()),
        sequence_number,
        fragment: id
            .fragment_info
            .as_ref()
            .map(|f| (f.offset, f.total_adu_length)),
    }
}

#[tokio::test]
async fn a_whole_bundle_gains_one_signed_receipt() {
    let (_bpa, cla, rx, key) = setup(Policy::Always).await;
    let payload = b"custody receipt payload".as_slice();
    arrive(&cla, plain(payload).1).await;

    let out = next(&rx).await;
    let (block, data) = custody_block(&out).expect("the custodian added block 224");
    assert_eq!(
        block.flags,
        Flags {
            must_replicate: true,
            ..Default::default()
        }
    );
    assert_eq!(block.crc_type, CrcType::CRC32_CASTAGNOLI);
    let items = receipt::items(data).unwrap();
    assert_eq!(items.len(), 1);
    let signed = receipt::verify(&key, items[0]).unwrap();
    assert!(signed.starts_with(&known_prefix(PEER, identity(&out, "ipn:3.1"), payload)));
    assert_eq!(out.bundle.blocks[&1].payload(&out.data).unwrap(), payload);
}

#[tokio::test]
async fn each_fragment_gets_a_receipt_naming_the_fragment() {
    let (_bpa, cla, rx, key) = setup(Policy::Always).await;
    let adu = b"0123456789abcdefghijklmnopqrstuvwxyz".as_slice();
    let total = adu.len() as u64;
    for (offset, slice) in [(0, &adu[..12]), (12, &adu[12..24]), (24, &adu[24..])] {
        arrive(&cla, fragment(slice, offset, total)).await;
        let out = next(&rx).await;
        let id = identity(&out, "ipn:3.1");
        assert_eq!(id.fragment, Some((offset, total)));
        let (block, data) = custody_block(&out).expect("the custodian added block 224");
        assert!(
            block.flags.must_replicate,
            "block 224 replicates into fragments"
        );
        let items = receipt::items(data).unwrap();
        let signed = receipt::verify(&key, items[0]).unwrap();
        assert!(signed.starts_with(&known_prefix(PEER, id, slice)));
    }
}

#[tokio::test]
async fn a_bcb_covered_payload_is_left_unsigned() {
    let (_bpa, cla, rx, _key) = setup(Policy::Always).await;
    let (bundle, data) = plain(b"secret payload");
    // Immaterial key: generated, and this node does not hold it.
    let mut key_bytes = vec![0u8; 32];
    SysRng.try_fill_bytes(&mut key_bytes).unwrap();
    let bcb_key = Key {
        key_type: KeyType::octet_sequence(key_bytes),
        key_algorithm: None,
        enc_algorithm: Some(EncAlgorithm::A256GCM),
        operations: Some(
            [Operation::Encrypt, Operation::Decrypt]
                .into_iter()
                .collect(),
        ),
        id: Some("ipn:3.1".into()),
        key_use: None,
    };
    let encrypted = Encryptor::new(&bundle, &data)
        .encrypt_block(
            1,
            Context::AES_GCM(Default::default()),
            SOURCE.parse().unwrap(),
            &bcb_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();
    arrive(&cla, encrypted).await;

    let out = next(&rx).await;
    assert!(
        out.bundle.blocks[&1].bcb.is_some(),
        "the payload is still under its BCB"
    );
    assert!(
        custody_block(&out).is_none(),
        "no receipt over a BCB-covered payload"
    );
}

#[tokio::test]
async fn block_present_signs_only_bundles_already_carrying_block_224() {
    let (_bpa, cla, rx, key) = setup(Policy::BlockPresent).await;

    arrive(&cla, plain(b"no block 224 here").1).await;
    let out = next(&rx).await;
    assert!(
        custody_block(&out).is_none(),
        "a plain bundle is forwarded unsigned"
    );

    // An earlier custodian's receipt, from another key. Only its signature
    // and bytes are checked here, so its fields are illustrative.
    let earlier_key = signing_key();
    let payload = b"block 224 present".as_slice();
    let earlier_fields = Fields {
        prev_hop: "ipn:3.1",
        this_hop: "ipn:3.0",
        bundle: BundleIdentity {
            source: "ipn:3.1",
            creation_time: 0,
            sequence_number: 0,
            fragment: None,
        },
        payload_hash: Sha256::digest(payload).into(),
        received_at: 1,
        forwarded_at: 2,
        subject: None,
    };
    let earlier = receipt::sign(&earlier_key, &earlier_fields.encode());
    let (_, data) = Builder::new(SOURCE.parse().unwrap(), REMOTE.parse().unwrap())
        .add_extension_block(Type::Unrecognised(BLOCK_TYPE))
        .unwrap()
        .with_flags(Flags {
            must_replicate: true,
            ..Default::default()
        })
        .with_crc_type(CrcType::CRC32_CASTAGNOLI)
        .build(receipt::new_block(&earlier).into())
        .with_payload(payload.into())
        .build(CreationTimestamp::now())
        .unwrap();
    arrive(&cla, data).await;

    let out = next(&rx).await;
    let (_, data) = custody_block(&out).expect("block 224 is still there");
    let items = receipt::items(data).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], &earlier[..], "the earlier receipt is unchanged");
    assert!(receipt::verify(&earlier_key.verifying_key(), items[0]).is_ok());
    let signed = receipt::verify(&key, items[1]).unwrap();
    assert!(signed.starts_with(&known_prefix(PEER, identity(&out, "ipn:3.1"), payload)));
}
