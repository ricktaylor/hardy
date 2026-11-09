//! BPA Pipeline Integration Tests
//!
//! These tests verify end-to-end bundle processing through the BPA,
//! covering the component test plan (PLAN-BPA-01) Suites A and B.

use core::{
    num::{NonZeroU8, NonZeroU32, NonZeroU64},
    ops::Range,
    time::Duration,
};
#[cfg(feature = "rfc9173")]
use hardy_bpa::bundle::{Bundle, BundleMetadata, BundleStatus};
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla,
    filter::{
        Classifier, ClassifyContext, RewriteContext, Rewriter, Verdict, pack::FilterPack,
        slots::MetadataDelta,
    },
    node_ids::NodeIds,
    routing::{RouteAction, RoutingAgent, RoutingSink, StaticRoutingAgent},
    services,
    storage::{
        BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage, RecoveryResponse,
        Result as StorageResult,
    },
    stream::{Receiver, Segment, Sender, buffer_stream},
};
// Aliased: this file names several crates' `Error`s.
use hardy_bpv7::{
    Error as Bpv7Error,
    block::{Flags as BlockFlags, Type},
    builder::Builder,
    bundle::{Flags, FragmentInfo, Id},
    crc::CrcType,
    creation_timestamp::CreationTimestamp,
    dtn_time::DtnTime,
    editor::{Chunk, Editor},
    eid::{
        Service, {Eid, IpnNodeId, NodeId},
    },
    hop_info::HopInfo,
    parse::{Parsed, parse},
    reader::Availability,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion},
};
// Aliased: the key type, beside the block `Type` imported above.
#[cfg(feature = "rfc9173")]
use hardy_bpa::keys::KeyProvider;
#[cfg(feature = "rfc9173")]
use hardy_bpv7::{
    block::BibCoverage,
    bpsec::{
        Role, Roles, SecurityServices,
        encryptor::{Context as EncryptContext, Encryptor},
        key::{EncAlgorithm, Key, KeyAlgorithm, KeySet, KeySource, Operation, Type as KeyType},
        rfc9173::ScopeFlags,
        signer::{self, Signer},
    },
};
use hardy_cbor::{
    decode::skip_value,
    encode::{Raw, emit, emit_array},
};
#[cfg(feature = "rfc9173")]
use rand::{TryRng, rngs::SysRng};
use std::{
    borrow::Cow,
    collections::VecDeque,
    env,
    slice::from_ref,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::available_parallelism,
};

mod common;

struct PipelineCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    forwarded_tx: flume::Sender<Bytes>,
}

impl PipelineCla {
    fn new() -> (Arc<Self>, flume::Receiver<Bytes>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                forwarded_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for PipelineCla {
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

// ---------------------------------------------------------------------------
// Test Application — receives delivered bundles via a channel
// ---------------------------------------------------------------------------

struct TestApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    received_tx: flume::Sender<(Eid, Bytes)>,
}

impl TestApp {
    fn new() -> (Arc<Self>, flume::Receiver<(Eid, Bytes)>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                received_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl services::Application for TestApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let payload = buffer_stream(stream, total_len).await?;
        self.received_tx
            .send((bundle_id.source.clone(), payload))
            .map_err(|e| services::Error::Internal(e.into()))
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Inline Echo Service — swaps source/destination and sends back
// ---------------------------------------------------------------------------

struct EchoService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
}

impl EchoService {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sink: hardy_async::sync::spin::Once::new(),
        })
    }
}

#[async_trait]
impl services::Service for EchoService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        let Some(sink) = self.sink.get() else {
            return Ok(());
        };

        let Ok(Parsed {
            data, bundle: raw, ..
        }) = parse(data)
        else {
            return Ok(());
        };
        let Ok(editor) = Editor::new(&raw, &data).with_source(raw.primary.destination.clone())
        else {
            return Ok(());
        };
        let Ok(editor) = editor.with_destination(raw.primary.id.source.clone()) else {
            return Ok(());
        };
        let Ok(chunks) = editor.rebuild() else {
            return Ok(());
        };

        let mut reply = Chunk::flatten_bytes(chunks, data);
        sink.send(&mut reply).await.map(|_| ())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Timed CLA — captures arrival time inside forward() for accurate benchmarking
// ---------------------------------------------------------------------------

struct TimedCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    arrival_tx: flume::Sender<tokio::time::Instant>,
}

impl TimedCla {
    fn new() -> (Arc<Self>, flume::Receiver<tokio::time::Instant>) {
        let (tx, rx) = flume::bounded(4096);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                arrival_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for TimedCla {
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
        _total_len: u64,
        _stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.arrival_tx.send(tokio::time::Instant::now());
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// ---------------------------------------------------------------------------
// Helper: print system info for benchmark context
// ---------------------------------------------------------------------------

fn print_system_info() {
    use std::fs;

    // CPU model
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo")
        && let Some(model) = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
    {
        eprintln!("CPU: {}", model.trim());
    }

    // Logical cores
    let cores = available_parallelism().map(|n| n.get()).unwrap_or(0);
    eprintln!("Cores: {cores}");

    // Total RAM
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo")
        && let Some(total) = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    {
        eprintln!("RAM: {} GB", total / 1_048_576);
    }

    // OS
    if let Ok(release) = fs::read_to_string("/etc/os-release")
        && let Some(pretty) = release
            .lines()
            .find(|l| l.starts_with("PRETTY_NAME"))
            .and_then(|l| l.split('=').nth(1))
    {
        eprintln!("OS: {}", pretty.trim_matches('"'));
    }

    eprintln!("Arch: {}", env::consts::ARCH);

    // Tokio runtime config (from the #[tokio::test] attribute)
    let rt_metrics = tokio::runtime::Handle::current().metrics();
    eprintln!("Tokio workers: {}", rt_metrics.num_workers());
    eprintln!(
        "Profile: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    eprintln!(
        "Date: {}",
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Helper: build a bundle as raw bytes
// ---------------------------------------------------------------------------

fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

// ---------------------------------------------------------------------------
// INT-BPA-01: App-to-CLA Routing
// ---------------------------------------------------------------------------

/// A locally-originated bundle addressed to a remote node is forwarded
/// to the correct CLA peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_to_cla_routing() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    // Register CLA and add a peer for the remote node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, &[remote_node])
        .await
        .unwrap();

    // Register an application to send from
    let (app, _app_rx) = TestApp::new();
    let source_eid = bpa
        .register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    // Send a bundle to the remote node
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            dest.clone(),
            Bytes::from_static(b"Hello remote"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The CLA should forward the bundle
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    // Parse and verify the forwarded bundle (structural — no keys).
    let Parsed {
        bundle: parsed_bundle,
        ..
    } = parse(forwarded).expect("Failed to parse forwarded bundle");

    assert_eq!(parsed_bundle.primary.id.source, source_eid);
    assert_eq!(parsed_bundle.primary.destination, dest);

    bpa.shutdown().await;
}

// Bundle storage that hands each saved bundle to the test, so it can see
// what is stored, and each deleted one to `deleted_tx`, where the test asks.
struct SavedBundles {
    inner: BundleMemStorage,
    saved_tx: flume::Sender<Bytes>,
    deleted_tx: Option<flume::Sender<Bytes>>,
}

impl SavedBundles {
    fn new() -> (Arc<Self>, flume::Receiver<Bytes>) {
        let (saved_tx, saved_rx) = flume::unbounded();
        (
            Arc::new(Self {
                inner: BundleMemStorage::new(None, None),
                saved_tx,
                deleted_tx: None,
            }),
            saved_rx,
        )
    }

    // As `new`, handing each deleted bundle to the second receiver too.
    #[cfg(feature = "rfc9173")]
    fn with_deletions() -> (Arc<Self>, flume::Receiver<Bytes>, flume::Receiver<Bytes>) {
        let (saved_tx, saved_rx) = flume::unbounded();
        let (deleted_tx, deleted_rx) = flume::unbounded();
        (
            Arc::new(Self {
                inner: BundleMemStorage::new(None, None),
                saved_tx,
                deleted_tx: Some(deleted_tx),
            }),
            saved_rx,
            deleted_rx,
        )
    }
}

#[async_trait]
impl BundleStorage for SavedBundles {
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> StorageResult<()> {
        self.inner.recover(stream).await
    }

    async fn load(&self, storage_name: &str) -> StorageResult<Option<Bytes>> {
        self.inner.load(storage_name).await
    }

    async fn save(&self, data: Bytes) -> StorageResult<Arc<str>> {
        let _ = self.saved_tx.send(data.clone());
        self.inner.save(data).await
    }

    async fn delete(&self, storage_name: &str) -> StorageResult<()> {
        if let Some(deleted_tx) = &self.deleted_tx
            && let Some(data) = self.inner.load(storage_name).await?
        {
            let _ = deleted_tx.send(data);
        }
        self.inner.delete(storage_name).await
    }
}

// The first bundle stored from `source`.
async fn stored_from(saved_rx: &flume::Receiver<Bytes>, source: &Eid) -> Parsed {
    loop {
        // Event-driven wait; the timeout only bounds a regression.
        let data = tokio::time::timeout(tokio::time::Duration::from_secs(5), saved_rx.recv_async())
            .await
            .expect("timeout waiting for the stored bundle")
            .expect("channel closed");
        let stored = parse(data).expect("a stored bundle parses");
        if &stored.bundle.primary.id.source == source {
            return stored;
        }
    }
}

// `base` with an unrecognised extension block (type 999, block 2) flagged
// `delete_block_on_failure` inserted between the primary and the payload:
// a block the header pass schedules for removal.
fn with_unknown_deletable_block(base: &[u8]) -> Bytes {
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x10u64); // flags: delete_block_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    Bytes::from(modified)
}

// A key provider lending the same keys for every bundle.
#[cfg(feature = "rfc9173")]
struct FixedKeys(Vec<Key>);

#[cfg(feature = "rfc9173")]
impl KeyProvider for FixedKeys {
    fn key_source(&self, _bundle: &hardy_bpv7::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
        Box::new(KeySet::new(self.0.clone()))
    }
}

// A signing key and an encryption key. Immaterial key values: generated per
// the no-literal-keys rule.
#[cfg(feature = "rfc9173")]
fn generated_keys() -> (Key, Key) {
    let generated = || {
        let mut k = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut k).unwrap();
        KeyType::octet_sequence(k)
    };
    let sign_key = Key {
        key_type: generated(),
        key_algorithm: Some(KeyAlgorithm::HS256),
        enc_algorithm: None,
        operations: Some([Operation::Sign, Operation::Verify].into_iter().collect()),
        id: None,
        key_use: None,
    };
    let enc_key = Key {
        key_type: generated(),
        key_algorithm: None,
        enc_algorithm: Some(EncAlgorithm::A256GCM),
        operations: Some(
            [Operation::Encrypt, Operation::Decrypt]
                .into_iter()
                .collect(),
        ),
        id: None,
        key_use: None,
    };
    (sign_key, enc_key)
}

// A bundle from `source` to `dest` whose previous node, `previous`, encrypted
// the Previous Node and Hop Count blocks for this hop and signed the Bundle
// Age block (5 s): the bundle, and the keys this node holds for them.
#[cfg(feature = "rfc9173")]
fn with_protected_per_hop_blocks(source: &Eid, dest: &Eid, previous: &Eid) -> (Bytes, Vec<Key>) {
    let (built, data) = Builder::new(source.clone(), dest.clone())
        .add_extension_block(Type::PreviousNode)
        .unwrap()
        .build(emit(previous).0.into())
        .add_extension_block(Type::BundleAge)
        .unwrap()
        .build(emit(&5_000u64).0.into())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let number = |block_type: Type| {
        *built
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block_type)
            .expect("the per-hop block is present")
            .0
    };

    let (sign_key, enc_key) = generated_keys();

    let signed = parse(Bytes::from(
        Signer::new(&built, &data)
            .sign_block(
                number(Type::BundleAge),
                signer::Context::HMAC_SHA2(Default::default()),
                previous.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the Bundle Age")
            .rebuild()
            .expect("rebuild the signed bundle"),
    ))
    .unwrap();
    let encrypted = Encryptor::new(&signed.bundle, &signed.data)
        .encrypt_block(
            number(Type::PreviousNode),
            EncryptContext::AES_GCM(Default::default()),
            previous.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the Previous Node")
        .encrypt_block(
            number(Type::HopCount),
            EncryptContext::AES_GCM(Default::default()),
            previous.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the Hop Count")
        .rebuild()
        .expect("rebuild the encrypted bundle");
    (Bytes::from(encrypted), vec![sign_key, enc_key])
}

// Ingress accepts the security operations on the per-hop blocks this node
// must replace, and on a Hop Count it holds the key for, before the bundle
// is first stored: the stored bundle carries each block in plaintext, with
// the value it arrived with, and no security block remains.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn accepted_per_hop_blocks_are_stored_in_plaintext() {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let previous: Eid = "ipn:0.4.0".parse().unwrap();
    let (inbound, keys) =
        with_protected_per_hop_blocks(&source, &"ipn:0.2.99".parse().unwrap(), &previous);

    let (bpa, saved_rx, _) = keyed_node_receiving(keys, None, inbound).await;

    let stored = stored_from(&saved_rx, &source).await;
    assert!(
        stored.bundle.blocks.values().all(|b| b.bcb.is_none()
            && b.bib == BibCoverage::None
            && !matches!(b.block_type, Type::BlockIntegrity | Type::BlockSecurity)),
        "every operation on the per-hop blocks is accepted"
    );
    assert_eq!(stored.per_hop.previous_node, Some(previous));
    assert_eq!(stored.per_hop.bundle_age.map(u64::from), Some(5_000));
    assert_eq!(stored.per_hop.hop_count.map(|h| h.count), Some(1));

    bpa.shutdown().await;
}

// A bundle without a creation time, from ipn:0.9.1 to REMOTE, requesting
// deletion reports, whose previous node protected its Bundle Age block
// (5 s): signed, or encrypted, by ipn:0.4.0, and, where `altered`, changed
// after so that the operation fails; with the key for it.
#[cfg(feature = "rfc9173")]
fn unclocked_with_protected_bundle_age(signed: bool, altered: bool) -> (Bytes, Key) {
    let previous: Eid = "ipn:0.4.0".parse().unwrap();
    let (built, data) = Builder::new("ipn:0.9.1".parse().unwrap(), REMOTE.parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:0.2.1".parse().unwrap())
        .add_extension_block(Type::BundleAge)
        .unwrap()
        // Without a CRC, as an RFC 9173 source leaves a target, so the
        // alteration reaches the security check.
        .with_crc_type(CrcType::None)
        .build(emit(&5_000u64).0.into())
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::default())
        .unwrap();
    let age = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::BundleAge)
        .expect("the Bundle Age is present")
        .0;
    let (sign_key, enc_key) = generated_keys();
    let (protected, key) = if signed {
        let signed = Signer::new(&built, &data)
            .sign_block(
                age,
                signer::Context::HMAC_SHA2(Default::default()),
                previous,
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the Bundle Age")
            .rebuild()
            .expect("rebuild the signed bundle");
        (signed, sign_key)
    } else {
        let encrypted = Encryptor::new(&built, &data)
            .encrypt_block(
                age,
                EncryptContext::AES_GCM(Default::default()),
                previous,
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the Bundle Age")
            .rebuild()
            .expect("rebuild the encrypted bundle");
        (encrypted, enc_key)
    };
    let mut inbound = Vec::from(protected);
    if altered {
        // The last byte of the block's data: the value changes, the CBOR
        // still decodes, and a ciphertext no longer authenticates.
        let parsed = parse(Bytes::copy_from_slice(&inbound)).unwrap();
        let last = usize::try_from(parsed.bundle.blocks[&age].payload_range().end).unwrap() - 1;
        inbound[last] ^= 0x01;
    }
    (Bytes::from(inbound), key)
}

// A bundle without a creation time cannot lose its Bundle Age, its only
// expiry signal (RFC 9171 §4.4.2), and this node is the acceptor of every
// operation on it: one it holds no key for deletes the bundle, "Block
// unsupported", and one that fails, "Failed security operation", where a
// bundle with a creation time would have the block replaced from the clock.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_unclocked_bundle_whose_bundle_age_it_cannot_accept_is_deleted() {
    for (signed, altered, reason) in [
        (false, false, ReasonCode::BlockUnsupported),
        (true, false, ReasonCode::BlockUnsupported),
        (false, true, ReasonCode::FailedSecurityOperation),
        (true, true, ReasonCode::FailedSecurityOperation),
    ] {
        let (inbound, key) = unclocked_with_protected_bundle_age(signed, altered);
        let keys = if altered { vec![key] } else { Vec::new() };
        let (bpa, saved_rx, sent_rx) = keyed_node_receiving(keys, None, inbound).await;
        assert_eq!(
            deletion_reason(&sent_rx).await,
            reason,
            "signed {signed}, altered {altered}"
        );
        bpa.shutdown().await;
        assert_never_stored(&saved_rx, &"ipn:0.9.1".parse().unwrap());
    }
}

// A destination behind node 1's peer, and one at node 1, for a service that
// has not registered.
#[cfg(feature = "rfc9173")]
const REMOTE: &str = "ipn:0.2.99";
#[cfg(feature = "rfc9173")]
const LOCAL: &str = "ipn:0.1.99";

// Node 1, storing through `SavedBundles`, holding `keys`, with `pack`'s
// filters, sending status reports, and with a CLA peer for node 2, once it
// has received `inbound` from that CLA: the node, the bundles it stored,
// and those it sent.
#[cfg(feature = "rfc9173")]
async fn keyed_node_receiving(
    keys: Vec<Key>,
    pack: Option<FilterPack>,
    inbound: Bytes,
) -> (Bpa, flume::Receiver<Bytes>, flume::Receiver<Bytes>) {
    let (bpa, saved_rx, _, sent_rx) = keyed_node_receiving_in(keys, pack, inbound, 1).await;
    (bpa, saved_rx, sent_rx)
}

// As `keyed_node_receiving`, the bundle sent in `segments` segments through
// a bounded channel with a spawned producer, so the header pass holds the
// headers and only a prefix of the payload: the node, the bundles it stored,
// those it deleted from storage, and those it sent.
#[cfg(feature = "rfc9173")]
async fn keyed_node_receiving_in(
    keys: Vec<Key>,
    pack: Option<FilterPack>,
    inbound: Bytes,
    segments: usize,
) -> (
    Bpa,
    flume::Receiver<Bytes>,
    flume::Receiver<Bytes>,
    flume::Receiver<Bytes>,
) {
    let (bpa, cla, saved_rx, deleted_rx, forwarded_rx) = keyed_node(keys, pack, None).await;
    receive_in(&cla, inbound, segments).await;
    (bpa, saved_rx, deleted_rx, forwarded_rx)
}

// The node `keyed_node_receiving_in` builds, before it receives anything,
// its metadata in `metadata` where given: the node, its CLA, the bundles it
// stored, those it deleted from storage, and those it sent.
#[cfg(feature = "rfc9173")]
async fn keyed_node(
    keys: Vec<Key>,
    pack: Option<FilterPack>,
    metadata: Option<Arc<dyn MetadataStorage>>,
) -> (
    Bpa,
    Arc<PipelineCla>,
    flume::Receiver<Bytes>,
    flume::Receiver<Bytes>,
    flume::Receiver<Bytes>,
) {
    keyed_node_under(None, keys, pack, metadata).await
}

// `keyed_node` under a bundle size cap.
#[cfg(feature = "rfc9173")]
async fn keyed_node_under(
    max_bundle_size: Option<NonZeroU64>,
    keys: Vec<Key>,
    pack: Option<FilterPack>,
    metadata: Option<Arc<dyn MetadataStorage>>,
) -> (
    Bpa,
    Arc<PipelineCla>,
    flume::Receiver<Bytes>,
    flume::Receiver<Bytes>,
    flume::Receiver<Bytes>,
) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let (saved, saved_rx, deleted_rx) = SavedBundles::with_deletions();
    let mut builder = Bpa::builder()
        .node_ids(node_ids)
        .bundle_storage(saved)
        .status_reports(true)
        .key_provider(Arc::new(FixedKeys(keys)));
    if let Some(metadata) = metadata {
        builder = builder.metadata_storage(metadata);
    }
    if let Some(pack) = pack {
        builder = builder.add_filters(pack);
    }
    if let Some(max_bundle_size) = max_bundle_size {
        builder = builder.max_bundle_size(max_bundle_size);
    }
    let bpa = builder.build().await.unwrap();
    bpa.start(false).await;
    let (cla, forwarded_rx) = PipelineCla::new();
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
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();
    (bpa, cla, saved_rx, deleted_rx, forwarded_rx)
}

// `inbound` received through `cla` in `segments` segments, through a bounded
// channel with a spawned producer, so the header pass holds the headers and
// only a prefix of the payload; the transfer is accepted.
#[cfg(feature = "rfc9173")]
async fn receive_in(cla: &PipelineCla, inbound: Bytes, segments: usize) {
    let step = inbound.len().div_ceil(segments);
    let mut split: Vec<Segment> = (0..inbound.len())
        .step_by(step)
        .map(|start| Segment::Next(inbound.slice(start..(start + step).min(inbound.len()))))
        .collect();
    if let Some(Segment::Next(last)) = split.pop() {
        split.push(Segment::Final(last));
    }
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    // A gate that rejects the bundle returns before draining the rest,
    // which the producer then stops sending.
    let producer = tokio::spawn(async move {
        for segment in split {
            if hardy_async::channel::Sender::send(&tx, segment)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut rx)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    drop(rx);
    producer.await.unwrap();
}

// Announces `role` on every security operation each block of its types
// carries: the services present on the block.
#[cfg(feature = "rfc9173")]
struct AnnouncesPresent {
    role: Role,
    block_types: Vec<Type>,
}

#[cfg(feature = "rfc9173")]
impl Classifier for AnnouncesPresent {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        for (&n, b) in &ctx.bundle().blocks {
            if !self.block_types.contains(&b.block_type) {
                continue;
            }
            let present = SecurityServices {
                confidentiality: b.bcb.is_some(),
                integrity: b.bib != BibCoverage::None,
            };
            delta.roles.insert(n, Roles::each(self.role, present));
        }
        Verdict::Continue(delta)
    }
}

// Announces the given roles on every block of each type.
#[cfg(feature = "rfc9173")]
struct AnnouncesRoles(Vec<(Type, Roles)>);

#[cfg(feature = "rfc9173")]
impl Classifier for AnnouncesRoles {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        for (&n, b) in &ctx.bundle().blocks {
            if let Some((_, roles)) = self.0.iter().find(|(t, _)| *t == b.block_type) {
                delta.roles.insert(n, *roles);
            }
        }
        Verdict::Continue(delta)
    }
}

// A filter pack announcing this node the acceptor of every security
// operation on every block of `block_types`.
#[cfg(feature = "rfc9173")]
fn accepting(block_types: &[Type]) -> Option<FilterPack> {
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier(
        "accepts",
        AnnouncesPresent {
            role: Role::Acceptor,
            block_types: block_types.to_vec(),
        },
    );
    Some(pack)
}

// A filter pack announcing this node the verifier of every security
// operation on every block of `block_types`.
#[cfg(feature = "rfc9173")]
fn verifying(block_types: &[Type]) -> Option<FilterPack> {
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier(
        "verifies",
        AnnouncesPresent {
            role: Role::Verifier,
            block_types: block_types.to_vec(),
        },
    );
    Some(pack)
}

// A filter pack announcing the given roles on every block of each type.
#[cfg(feature = "rfc9173")]
fn with_roles(roles: &[(Type, Roles)]) -> Option<FilterPack> {
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier("roles", AnnouncesRoles(roles.to_vec()));
    Some(pack)
}

// The first status report the node sends.
#[cfg(feature = "rfc9173")]
async fn sent_status_report(sent_rx: &flume::Receiver<Bytes>) -> BundleStatusReport {
    loop {
        // Event-driven wait; the timeout only bounds a regression.
        let sent = tokio::time::timeout(Duration::from_secs(5), sent_rx.recv_async())
            .await
            .expect("timeout waiting for the status report")
            .expect("channel closed");
        let sent = parse(sent).expect("a sent bundle parses");
        if !sent.bundle.primary.flags.is_admin_record {
            continue;
        }
        let body = sent.bundle.blocks[&1]
            .payload(&sent.data)
            .expect("the report's payload is resident");
        let AdministrativeRecord::BundleStatusReport(status) =
            hardy_cbor::decode::parse(body).expect("the payload is an admin record");
        return status;
    }
}

// Of what the node stored, none is `source`'s: the caller's completed
// shutdown is the barrier.
#[cfg(feature = "rfc9173")]
fn assert_never_stored(saved_rx: &flume::Receiver<Bytes>, source: &Eid) {
    assert!(
        saved_rx.try_iter().all(|data| parse(data)
            .expect("a stored bundle parses")
            .bundle
            .primary
            .id
            .source
            != *source),
        "the deleted bundle is never stored"
    );
}

// A bundle from ipn:0.9.1 to `destination`, with the bundle flags
// `bundle_flags`, reporting to ipn:0.2.1, which the receiving node routes,
// carrying an extension block of type 199 with the block flags `flags` and
// no CRC, so that an alteration of its data reaches the security checks:
// the bundle as parsed, and the block's number.
#[cfg(feature = "rfc9173")]
fn with_block_199(destination: &str, flags: BlockFlags, bundle_flags: Flags) -> (Parsed, u64) {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (built, data) = Builder::new(source, destination.parse().unwrap())
        .with_flags(bundle_flags)
        .with_report_to("ipn:0.2.1".parse().unwrap())
        .add_extension_block(Type::Unrecognised(199))
        .unwrap()
        .with_flags(flags)
        .with_crc_type(CrcType::None)
        .build(b"announced".as_slice().into())
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let block = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::Unrecognised(199))
        .expect("the block is present")
        .0;
    (parse(Bytes::from(data)).unwrap(), block)
}

// The scope under which one source's results share one security block:
// without the security header in them.
#[cfg(feature = "rfc9173")]
fn shared_scope() -> ScopeFlags {
    ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    }
}

// `parsed` with `targets` signed by its source, ipn:0.9.1, with `key`, into
// one BIB.
#[cfg(feature = "rfc9173")]
fn signed_by_source(parsed: &Parsed, targets: &[u64], key: &Key) -> Parsed {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let mut signing = Signer::new(&parsed.bundle, &parsed.data);
    for &n in targets {
        signing = signing
            .sign_block(
                n,
                signer::Context::HMAC_SHA2(shared_scope()),
                source.clone(),
                key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    parse(Bytes::from(signing.rebuild().expect("rebuild signed"))).unwrap()
}

// A bundle from ipn:0.9.1 to `destination` carrying an extension block of
// type 199, with `flags`, which its source signed, with the payload too
// where `with_payload`, into one BIB, and encrypted nothing: the bundle,
// the block's number, and the signing key.
#[cfg(feature = "rfc9173")]
fn with_signed_block(
    destination: &str,
    flags: BlockFlags,
    with_payload: bool,
    bundle_flags: Flags,
) -> (Bytes, u64, Key) {
    let (parsed, block) = with_block_199(destination, flags, bundle_flags);
    let targets: Vec<u64> = if with_payload {
        vec![block, 1]
    } else {
        vec![block]
    };
    let (sign_key, _) = generated_keys();
    let signed = signed_by_source(&parsed, &targets, &sign_key);
    (signed.data, block, sign_key)
}

// A bundle from ipn:0.9.1 to `destination` carrying an extension block of
// type 199, with `flags`, which its source encrypted, having first signed it,
// with the payload too where `with_payload`, into one BIB, which the
// encryption then covers: the bundle, the block's number, and the keys.
#[cfg(feature = "rfc9173")]
fn with_protected_block(
    destination: &str,
    flags: BlockFlags,
    signed: bool,
    with_payload: bool,
) -> (Bytes, u64, Vec<Key>) {
    protected_block(destination, flags, signed, with_payload, false)
}

// As `with_protected_block`, the block's data altered after signing, where
// `altered`, so the BIB's operation on it fails.
#[cfg(feature = "rfc9173")]
fn protected_block(
    destination: &str,
    flags: BlockFlags,
    signed: bool,
    with_payload: bool,
    altered: bool,
) -> (Bytes, u64, Vec<Key>) {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (parsed, block) = with_block_199(destination, flags, Flags::default());
    let targets: Vec<u64> = if with_payload {
        vec![block, 1]
    } else {
        vec![block]
    };
    let (sign_key, enc_key) = generated_keys();
    let signed = if signed {
        signed_by_source(&parsed, &targets, &sign_key)
    } else {
        parsed
    };
    let signed = if altered {
        // The block carries no CRC, so the altered block still parses and
        // the alteration reaches the security checks.
        let start = usize::try_from(signed.bundle.blocks[&block].payload_range().start).unwrap();
        let mut data = signed.data.to_vec();
        data[start] ^= 0x01;
        parse(Bytes::from(data)).unwrap()
    } else {
        signed
    };
    let mut encrypting = Encryptor::new(&signed.bundle, &signed.data);
    for &n in &targets {
        encrypting = encrypting
            .encrypt_block(
                n,
                EncryptContext::AES_GCM(shared_scope()),
                source.clone(),
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the target");
    }
    (
        Bytes::from(encrypting.rebuild().expect("rebuild encrypted")),
        block,
        vec![sign_key, enc_key],
    )
}

// A block a Classifier announces is accepted at the gate: its BCB, the BCB
// over the encrypted BIB that covers it with it, and its BIB operation are
// processed and removed before the bundle is first stored, which carries the
// block in plaintext and no security block.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_block_is_stored_without_its_operations() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, false);

    let (bpa, saved_rx, _) =
        keyed_node_receiving(keys, accepting(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        stored.bundle.blocks.values().all(|b| b.bcb.is_none()
            && b.bib == BibCoverage::None
            && !matches!(b.block_type, Type::BlockIntegrity | Type::BlockSecurity)),
        "every operation on the announced block is accepted"
    );
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        Some(b"announced".as_slice())
    );
    bpa.shutdown().await;
}

// An announced block whose BCB this node holds no key for is a failed
// acceptance, a block it cannot process (RFC 9171 §5.6 Step 4), which its
// flags leave to be discarded with its operations.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_block_without_its_key_is_discarded() {
    let (inbound, block, _) = with_protected_block(REMOTE, BlockFlags::default(), false, false);

    let (bpa, saved_rx, _) =
        keyed_node_receiving(Vec::new(), accepting(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    assert!(
        stored.bundle.blocks.values().all(|b| b.bcb.is_none()),
        "with its BCB"
    );
    bpa.shutdown().await;
}

// A block announced for acceptance that carries no security operation is a
// failed acceptance, since the announcement expects one: it is discarded,
// as RFC 9172 §5.1.1 and §5.1.2 recommend, and its flags demand the
// reception report, "Missing security operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_block_without_a_security_operation_is_discarded() {
    let (received, block) = with_block_199(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        Flags::default(),
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        Vec::new(),
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Acceptor, SecurityServices::BOTH),
        )]),
        received.data,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::MissingSecurityOperation);
    bpa.shutdown().await;
}

// An announced payload that carries no security operation deletes the
// bundle, which cannot lose its payload, reported "Missing security
// operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_payload_without_a_security_operation_deletes_the_bundle() {
    let (received, _) = with_block_199(
        REMOTE,
        BlockFlags::default(),
        Flags {
            delete_report_requested: true,
            ..Flags::default()
        },
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        Vec::new(),
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Acceptor, SecurityServices::BOTH),
        )]),
        received.data,
    )
    .await;

    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::MissingSecurityOperation
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.9.1".parse().unwrap());
}

// An announced block whose encrypted BIB also covers the payload cannot be
// accepted here, since the payload's BCB operation goes with the block's: a
// failed acceptance. Removing it from that BIB would mean encrypting the BIB
// again, which only its BCB's security source may do, and no RFC requires
// it gone, so it stays as received rather than strip the payload's
// integrity.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_block_tied_to_the_payload_is_a_failed_acceptance() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, true);
    let received = parse(inbound.clone()).unwrap();

    let (bpa, saved_rx, _) =
        keyed_node_receiving(keys, accepting(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        received.bundle.blocks[&block].payload(&received.data),
        "the block stays as received"
    );
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::BlockIntegrity),
        "with the BIB"
    );
    assert!(
        stored.bundle.blocks[&1].bcb.is_some(),
        "the payload travels encrypted"
    );
    bpa.shutdown().await;
}

// With the payload announced too, a block whose encrypted BIB also covers
// it is accepted with it at a relay, whether the payload arrives whole or in
// segments: both are stored in plaintext, and no security block remains.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_block_tied_to_an_announced_payload_is_accepted_with_it() {
    for segments in [1, 3] {
        let (inbound, block, keys) =
            with_protected_block(REMOTE, BlockFlags::default(), true, true);
        let pack = accepting(&[Type::Unrecognised(199), Type::Payload]);

        let (bpa, saved_rx, _, _) = keyed_node_receiving_in(keys, pack, inbound, segments).await;

        let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
        assert!(
            stored.bcbs.is_empty() && stored.bibs.is_empty(),
            "{segments} segments: every operation is accepted"
        );
        assert_eq!(
            stored.bundle.blocks[&block].payload(&stored.data),
            Some(b"announced".as_slice()),
            "{segments} segments"
        );
        assert_eq!(
            stored.bundle.blocks[&1].payload(&stored.data),
            Some(b"payload".as_slice()),
            "{segments} segments"
        );
        bpa.shutdown().await;
    }
}

// A block a Classifier designates this node to verify, whose BIB operation
// the header pass checks with the key it holds, travels as received: a
// verifier changes nothing (RFC 9172 §5.1.2).
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_this_node_verifies_travels_as_received() {
    let (inbound, block, key) =
        with_signed_block(REMOTE, BlockFlags::default(), false, Flags::default());
    let received = parse(inbound.clone()).unwrap();

    let (bpa, saved_rx, _) =
        keyed_node_receiving(vec![key], verifying(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        received.bundle.blocks[&block].payload(&received.data),
        "the block stays as received"
    );
    assert_eq!(
        stored.bundle.blocks[&block].bib, received.bundle.blocks[&block].bib,
        "under its BIB"
    );
    bpa.shutdown().await;
}

// A designated block whose BIB operation this node holds no key for is a
// failed verification, a block it cannot process (RFC 9171 §5.6 Step 4):
// its flags leave it to be discarded with its operation, and demand the
// reception report, "Block unsupported".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_without_its_key_is_discarded() {
    let (inbound, block, _) = with_signed_block(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        false,
        Flags::default(),
    );

    let (bpa, saved_rx, sent_rx) =
        keyed_node_receiving(Vec::new(), verifying(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .all(|b| b.block_type != Type::BlockIntegrity),
        "with its BIB"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);
    bpa.shutdown().await;
}

// Where the designated block's flags say "delete bundle if block can't be
// processed", the failed verification deletes the bundle, reported "Block
// unsupported".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_without_its_key_deletes_the_bundle_its_flags_protect() {
    let (inbound, _, _) = with_signed_block(
        REMOTE,
        BlockFlags {
            delete_bundle_on_failure: true,
            ..BlockFlags::default()
        },
        false,
        Flags {
            delete_report_requested: true,
            ..Flags::default()
        },
    );

    let (bpa, saved_rx, sent_rx) =
        keyed_node_receiving(Vec::new(), verifying(&[Type::Unrecognised(199)]), inbound).await;

    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::BlockUnsupported
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.9.1".parse().unwrap());
}

// A designated block that carries no integrity operation is discarded, as
// RFC 9172 §5.1.2 recommends for a target that policy expected integrity
// on, and its flags demand the reception report, "Missing security
// operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_without_an_integrity_operation_is_discarded() {
    let (received, block) = with_block_199(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        Flags::default(),
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        Vec::new(),
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
        )]),
        received.data,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::MissingSecurityOperation);
    bpa.shutdown().await;
}

// A designated payload that carries no integrity operation deletes the
// bundle, which cannot lose its payload, reported "Missing security
// operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_payload_without_an_integrity_operation_deletes_the_bundle() {
    let (received, _) = with_block_199(
        REMOTE,
        BlockFlags::default(),
        Flags {
            delete_report_requested: true,
            ..Flags::default()
        },
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        Vec::new(),
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
        )]),
        received.data,
    )
    .await;

    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::MissingSecurityOperation
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.9.1".parse().unwrap());
}

// A designated block whose BIB operation is in a security context this node
// does not support is a failed verification, and the BIB is a block it
// cannot process too: the target's flags discard it, the BIB's discard the
// BIB, and the BIB's report demand names the reason, "Unknown security
// operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_in_an_unsupported_context_is_discarded_with_its_bib() {
    let (received, block) = with_block_199(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        Flags::default(),
    );
    // A BIB with an unrecognised security context over the block, flagged
    // to report and to be discarded if it cannot be processed.
    let inbound = splice_unrecognised_security_block(&received.data, 11, 7, block, 0x12);

    let (bpa, saved_rx, sent_rx) =
        keyed_node_receiving(Vec::new(), verifying(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    assert!(
        !stored.bundle.blocks.contains_key(&7),
        "and the BIB with it"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::UnknownSecurityOperation);
    bpa.shutdown().await;
}

// A designated block this node also accepts is verified by its acceptance:
// the designation adds nothing, and the block is stored in plaintext
// without its operations, not discarded for the BCB it arrived under.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_this_node_accepts_is_verified_by_its_acceptance() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, false);
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier(
        "accepts",
        AnnouncesPresent {
            role: Role::Acceptor,
            block_types: vec![Type::Unrecognised(199)],
        },
    );
    pack.ingress_classifier(
        "verifies",
        AnnouncesPresent {
            role: Role::Verifier,
            block_types: vec![Type::Unrecognised(199)],
        },
    );

    let (bpa, saved_rx, _) = keyed_node_receiving(keys, Some(pack), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        Some(b"announced".as_slice()),
        "the block is accepted"
    );
    assert!(
        stored.bundle.blocks.values().all(|b| b.bcb.is_none()
            && b.bib == BibCoverage::None
            && !matches!(b.block_type, Type::BlockIntegrity | Type::BlockSecurity)),
        "without its operations"
    );
    bpa.shutdown().await;
}

// A designated payload whose BIB operation this node holds no key for
// deletes the bundle, of which the payload cannot be lost, reported "Block
// unsupported": whether the payload is resident at the header pass, which
// checks it there, or streams past it, where the drain would have.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_payload_without_its_key_deletes_the_bundle() {
    // Resident: the whole bundle in one segment.
    let (inbound, _, _) = with_signed_block(
        REMOTE,
        BlockFlags::default(),
        true,
        Flags {
            delete_report_requested: true,
            ..Flags::default()
        },
    );
    let (bpa, saved_rx, sent_rx) =
        keyed_node_receiving(Vec::new(), verifying(&[Type::Payload]), inbound).await;
    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::BlockUnsupported,
        "resident payload"
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.9.1".parse().unwrap());

    // Streaming: a 6 KiB payload in segments, of which only a prefix is
    // resident at the header pass.
    let (inbound, _, _) = with_protected_payload(REMOTE, true, false);
    let (bpa, saved_rx, _, sent_rx) =
        keyed_node_receiving_in(Vec::new(), verifying(&[Type::Payload]), inbound, 3).await;
    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::BlockUnsupported,
        "streaming payload"
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.2.1".parse().unwrap());
}

// A designated payload that streams past the header pass is checked by the
// drain with the key this node holds, and the bundle is stored as received.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_payload_is_verified_as_it_streams() {
    let (inbound, plaintext, keys) = with_protected_payload(REMOTE, true, false);

    let (bpa, saved_rx, _, _) = keyed_node_receiving_in(
        vec![keys[0].clone()],
        verifying(&[Type::Payload]),
        inbound,
        3,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&1].payload(&stored.data),
        Some(plaintext.as_slice()),
        "the payload is stored as received"
    );
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::BlockIntegrity),
        "under its BIB"
    );
    bpa.shutdown().await;
}

// A bundle from ipn:0.9.1 to a remote node carrying extension blocks of
// types 199 and 200, which its source signed into one BIB and then
// encrypted, so the BIB is encrypted with them: the bundle, the two blocks'
// numbers, and the keys.
#[cfg(feature = "rfc9173")]
fn with_blocks_signed_together() -> (Bytes, u64, u64, Vec<Key>) {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (built, data) = Builder::new(source.clone(), REMOTE.parse().unwrap())
        .add_extension_block(Type::Unrecognised(199))
        .unwrap()
        .build(b"announced".as_slice().into())
        .add_extension_block(Type::Unrecognised(200))
        .unwrap()
        .build(b"unannounced".as_slice().into())
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let number = |block_type| {
        *built
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block_type)
            .expect("the block is present")
            .0
    };
    let targets = [
        number(Type::Unrecognised(199)),
        number(Type::Unrecognised(200)),
    ];
    let (sign_key, enc_key) = generated_keys();
    // Without the security header in scope, the results share one BIB.
    let scope = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut signing = Signer::new(&built, &data);
    for n in targets {
        signing = signing
            .sign_block(
                n,
                signer::Context::HMAC_SHA2(scope.clone()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    let signed = parse(Bytes::from(signing.rebuild().expect("rebuild signed"))).unwrap();
    let mut encrypting = Encryptor::new(&signed.bundle, &signed.data);
    for n in targets {
        encrypting = encrypting
            .encrypt_block(
                n,
                EncryptContext::AES_GCM(scope.clone()),
                source.clone(),
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the target");
    }
    (
        Bytes::from(encrypting.rebuild().expect("rebuild encrypted")),
        targets[0],
        targets[1],
        vec![sign_key, enc_key],
    )
}

// An announced block whose encrypted BIB also covers a block no Classifier
// announces cannot be accepted, since the BCB operation on that block goes
// with the announced one's (RFC 9172 §3.9) and is not this node's to remove:
// a failed acceptance. Removing it from that BIB would mean encrypting the
// BIB again, and no RFC requires it gone, so it stays as received, and the
// other block keeps its ciphertext.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_block_tied_to_an_unannounced_block_is_a_failed_acceptance() {
    let (inbound, announced, other, keys) = with_blocks_signed_together();
    let received = parse(inbound.clone()).unwrap();

    let (bpa, saved_rx, _) =
        keyed_node_receiving(keys, accepting(&[Type::Unrecognised(199)]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&announced].payload(&stored.data),
        received.bundle.blocks[&announced].payload(&received.data),
        "the announced block stays as received"
    );
    assert!(
        stored.bundle.blocks[&other].bcb.is_some(),
        "the other block stays encrypted"
    );
    assert_eq!(
        stored.bundle.blocks[&other].payload(&stored.data),
        received.bundle.blocks[&other].payload(&received.data),
        "with its ciphertext"
    );
    bpa.shutdown().await;
}

// A block encrypted here cannot be accepted for integrity alone even where
// the BIB over it is readable, decrypted by necessity for a neighbour under
// the same BIB accepted for both services: verifying its integrity needs
// its plaintext, which only the acceptor of its BCB operation reads. Its
// acceptance is incomplete, and so is the neighbour's, coupled to a block
// not accepted for confidentiality: both are discarded, and the BIB they
// emptied and their BCBs go with them.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_accepted_for_integrity_alone_fails_with_its_coupled_neighbour() {
    let (inbound, both, integrity_only, keys) = with_blocks_signed_together();

    let (bpa, saved_rx, _) = keyed_node_receiving(
        keys,
        with_roles(&[
            (
                Type::Unrecognised(199),
                Roles::each(Role::Acceptor, SecurityServices::BOTH),
            ),
            (
                Type::Unrecognised(200),
                Roles::each(Role::Acceptor, SecurityServices::INTEGRITY),
            ),
        ]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    for (number, what) in [
        (integrity_only, "the integrity-only block"),
        (both, "its neighbour"),
    ] {
        assert!(
            !stored.bundle.blocks.contains_key(&number),
            "{what} is discarded"
        );
    }
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .all(|b| !matches!(b.block_type, Type::BlockIntegrity | Type::BlockSecurity)),
        "with their security blocks"
    );
    bpa.shutdown().await;
}

// A Hop Count its source signed into one BIB with the payload, in
// plaintext, couples nothing: a relay holding the key accepts it, and stores
// it without its BIB operation, which the payload's keeps.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_accepts_a_hop_count_sharing_a_plaintext_bib_with_the_payload() {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (built, data) = Builder::new(source.clone(), REMOTE.parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let hop_count = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::HopCount)
        .expect("the Hop Count is present")
        .0;
    let (sign_key, _) = generated_keys();
    // Without the security header in scope, the results share one BIB.
    let scope = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut signing = Signer::new(&built, &data);
    for n in [hop_count, 1] {
        signing = signing
            .sign_block(
                n,
                signer::Context::HMAC_SHA2(scope.clone()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    let inbound = Bytes::from(signing.rebuild().expect("rebuild signed"));

    let (bpa, saved_rx, _) = keyed_node_receiving(vec![sign_key], None, inbound).await;

    let stored = stored_from(&saved_rx, &source).await;
    assert_eq!(
        stored.bundle.blocks[&hop_count].bib,
        BibCoverage::None,
        "the Hop Count's operation is accepted"
    );
    assert!(
        matches!(stored.bundle.blocks[&1].bib, BibCoverage::Some(_)),
        "the payload's stays"
    );
    bpa.shutdown().await;
}

// A bundle from ipn:0.9.1 to a remote node carrying a Hop Count and an
// extension block of type 199, neither carrying a CRC, whose source signed
// the Hop Count and the `partner` block into one BIB and then encrypted
// them, so the BIB is encrypted with them: the bundle, the Hop Count's
// number, the partner's, and the signing and encryption keys.
#[cfg(feature = "rfc9173")]
fn with_hop_count_signed_with(partner: Type) -> (Bytes, u64, u64, Key, Key) {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (built, data) = Builder::new(source.clone(), REMOTE.parse().unwrap())
        // The Hop Count carries no CRC, as RFC 9173 §3.8.1 and §4.8.1 leave a
        // target, so that an alteration of its data reaches the security
        // check rather than the CRC.
        .add_extension_block(Type::HopCount)
        .unwrap()
        .with_flags(BlockFlags {
            report_on_failure: true,
            must_replicate: true,
            ..Default::default()
        })
        .with_crc_type(CrcType::None)
        .build(
            emit(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .0
            .into(),
        )
        .add_extension_block(Type::Unrecognised(199))
        .unwrap()
        .with_crc_type(CrcType::None)
        .build(b"partner".as_slice().into())
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let number = |block_type| {
        *built
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block_type)
            .expect("the block is present")
            .0
    };
    let (hop_count, partner) = (number(Type::HopCount), number(partner));
    let (sign_key, enc_key) = generated_keys();
    // Without the security header in scope, the results share one BIB.
    let scope = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut signing = Signer::new(&built, &data);
    for n in [hop_count, partner] {
        signing = signing
            .sign_block(
                n,
                signer::Context::HMAC_SHA2(scope.clone()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    let signed = parse(Bytes::from(signing.rebuild().expect("rebuild signed"))).unwrap();
    let mut encrypting = Encryptor::new(&signed.bundle, &signed.data);
    for n in [hop_count, partner] {
        encrypting = encrypting
            .encrypt_block(
                n,
                EncryptContext::AES_GCM(scope.clone()),
                source.clone(),
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the target");
    }
    (
        Bytes::from(encrypting.rebuild().expect("rebuild encrypted")),
        hop_count,
        partner,
        sign_key,
        enc_key,
    )
}

// A Hop Count its source signed with the payload and then encrypted with
// it, so that one encrypted BIB covers both, is accepted at a relay whose
// Classifier announces the payload, whether the payload arrives whole or in
// segments: the relay accepts every block that BIB covers, so the BCB
// operations on them all go together (RFC 9172 §3.9), and both are stored
// in plaintext with no security block.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_accepting_the_payload_accepts_a_hop_count_tied_to_it() {
    for segments in [1, 3] {
        let (inbound, hop_count, _, sign_key, enc_key) = with_hop_count_signed_with(Type::Payload);

        let (bpa, saved_rx, _, _) = keyed_node_receiving_in(
            vec![sign_key, enc_key],
            accepting(&[Type::Payload]),
            inbound,
            segments,
        )
        .await;

        let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
        assert!(
            stored.bcbs.is_empty() && stored.bibs.is_empty(),
            "{segments} segments: every operation is accepted"
        );
        assert_eq!(
            stored.bundle.blocks[&1].payload(&stored.data),
            Some(b"payload".as_slice()),
            "{segments} segments"
        );
        assert!(
            stored.bundle.blocks[&hop_count].bcb.is_none(),
            "{segments} segments: the Hop Count is stored in plaintext"
        );
        bpa.shutdown().await;
    }
}

// A relay without the signing key cannot check a Hop Count's operation in
// the encrypted BIB it shares with an announced block, so it does not join
// that block's acceptance: the announced block, tied to a block this node
// does not accept, is a failed acceptance, which stays as received since
// removing it would shrink that BIB, and the Hop Count, whose increment is a
// SHOULD, stays as received too.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_keeps_a_hop_count_it_cannot_check() {
    let (inbound, hop_count, partner, _, enc_key) =
        with_hop_count_signed_with(Type::Unrecognised(199));

    let (bpa, saved_rx, _) = keyed_node_receiving(
        vec![enc_key],
        accepting(&[Type::Unrecognised(199)]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        stored.bundle.blocks[&partner].bcb.is_some()
            && stored
                .bundle
                .blocks
                .values()
                .any(|b| b.block_type == Type::BlockIntegrity),
        "the announced block stays as received, with the BIB"
    );
    assert!(
        stored.bundle.blocks[&hop_count].bcb.is_some(),
        "the Hop Count stays as received"
    );
    bpa.shutdown().await;
}

// A block this node cannot process, flagged to be discarded, under an
// encrypted BIB that also covers the payload: RFC 9171 §5.6 Step 4 requires
// it removed, so a relay holding the keys removes that BIB with it, whole,
// rather than encrypt the BIB again, and the payload keeps its ciphertext.
// A relay without them cannot read the BIB, which may cover the block, and
// keeps the block as received rather than leave the BIB naming a block
// that no longer exists.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_removes_a_discard_flagged_block_with_its_shared_encrypted_bib() {
    let flags = BlockFlags {
        delete_block_on_failure: true,
        ..BlockFlags::default()
    };
    let (inbound, block, keys) = with_protected_block(REMOTE, flags, true, true);
    let received = parse(inbound.clone()).unwrap();

    let (bpa, saved_rx, _) = keyed_node_receiving(keys, None, inbound.clone()).await;
    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block)
            && !stored
                .bundle
                .blocks
                .values()
                .any(|b| b.block_type == Type::BlockIntegrity),
        "the block is removed with the BIB"
    );
    assert_eq!(
        stored.bundle.blocks[&1].payload(&stored.data),
        received.bundle.blocks[&1].payload(&received.data),
        "the payload keeps its ciphertext"
    );
    bpa.shutdown().await;

    let (bpa, saved_rx, _) = keyed_node_receiving(Vec::new(), None, inbound).await;
    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        received.bundle.blocks[&block].payload(&received.data),
        "without the keys the block stays as received"
    );
    bpa.shutdown().await;
}

// A Hop Count whose ciphertext does not authenticate is discarded with every
// security block associated with it (RFC 9172 §5.1.1): the encrypted BIB it
// shares with another block goes too, whole, so nothing is encrypted again,
// and the other block keeps its ciphertext without that BIB's operation.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_discards_an_undecryptable_hop_count_with_its_encrypted_bib() {
    let (inbound, hop_count, partner, sign_key, enc_key) =
        with_hop_count_signed_with(Type::Unrecognised(199));
    let received = parse(inbound.clone()).unwrap();
    let bib = *received
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::BlockIntegrity)
        .expect("one encrypted BIB covers both blocks")
        .0;
    let last = usize::try_from(received.bundle.blocks[&hop_count].payload_range().end).unwrap() - 1;
    let mut tampered = inbound.to_vec();
    tampered[last] ^= 0x01;

    let (bpa, saved_rx, _) =
        keyed_node_receiving(vec![sign_key, enc_key], None, Bytes::from(tampered)).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&hop_count) && !stored.bundle.blocks.contains_key(&bib),
        "the Hop Count is discarded with its BIB"
    );
    assert_eq!(
        stored.bundle.blocks[&partner].payload(&stored.data),
        received.bundle.blocks[&partner].payload(&received.data),
        "the other block keeps its ciphertext"
    );
    bpa.shutdown().await;
}

// A Hop Count with an operation in a security context this node does not
// support is not its to accept: the relay stores it with that operation,
// which it has not checked.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_leaves_a_hop_count_under_an_unsupported_bib() {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (built, data) = Builder::new(source.clone(), REMOTE.parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let hop_count = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::HopCount)
        .expect("the Hop Count is present")
        .0;
    let inbound = splice_unrecognised_security_block(&data, 11, 9, hop_count, 0);

    let (bpa, saved_rx, _) = keyed_node_receiving(Vec::new(), None, inbound).await;

    let stored = stored_from(&saved_rx, &source).await;
    assert_eq!(
        stored.bundle.blocks[&hop_count].bib,
        BibCoverage::Some(9),
        "the unsupported operation stays"
    );
    bpa.shutdown().await;
}

// The bundle's destination is the acceptor of every security operation in it
// (RFC 9172 §1.4): with no Classifier, a bundle for a local service, here one
// that has not registered, is stored with its encrypted, signed block in
// plaintext and no security block.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_accepts_every_operation() {
    let (inbound, block, keys) = with_protected_block(LOCAL, BlockFlags::default(), true, false);

    let (bpa, saved_rx, _) = keyed_node_receiving(keys, None, inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        stored.bundle.blocks.values().all(|b| b.bcb.is_none()
            && b.bib == BibCoverage::None
            && !matches!(b.block_type, Type::BlockIntegrity | Type::BlockSecurity)),
        "every operation is accepted"
    );
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        Some(b"announced".as_slice())
    );
    bpa.shutdown().await;
}

// At the destination, a block whose encrypted BIB also covers the payload
// is accepted with the payload, whether the payload arrives whole or in
// segments: its BCB goes with the payload's, all or none (RFC 9172 §3.9),
// and the BIB's operation on it is checked over its plaintext, so the
// bundle is stored with the block and the payload in plaintext and no
// security block.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_accepts_a_block_tied_to_the_payload() {
    for segments in [1, 3] {
        let (inbound, block, keys) = with_protected_block(LOCAL, BlockFlags::default(), true, true);

        let (bpa, saved_rx, _, _) = keyed_node_receiving_in(keys, None, inbound, segments).await;

        let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
        assert!(
            stored.bcbs.is_empty() && stored.bibs.is_empty(),
            "{segments} segments: every operation is accepted"
        );
        assert_eq!(
            stored.bundle.blocks[&block].payload(&stored.data),
            Some(b"announced".as_slice()),
            "{segments} segments"
        );
        assert_eq!(
            stored.bundle.blocks[&1].payload(&stored.data),
            Some(b"payload".as_slice()),
            "{segments} segments"
        );
        bpa.shutdown().await;
    }
}

// A block tied to the payload whose BIB operation fails at the destination
// is a failed security operation on it: its flags discard it, the payload
// still accepted, or delete the bundle, which nothing then stores.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_checks_a_block_tied_to_the_payload() {
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    for segments in [1, 3] {
        let (inbound, block, keys) =
            protected_block(LOCAL, BlockFlags::default(), true, true, true);
        let (bpa, saved_rx, _, _) = keyed_node_receiving_in(keys, None, inbound, segments).await;
        let stored = stored_from(&saved_rx, &source).await;
        assert!(
            !stored.bundle.blocks.contains_key(&block),
            "{segments} segments: the block is discarded"
        );
        assert!(
            stored.bcbs.is_empty() && stored.bibs.is_empty(),
            "{segments} segments: with its operations"
        );
        assert_eq!(
            stored.bundle.blocks[&1].payload(&stored.data),
            Some(b"payload".as_slice()),
            "{segments} segments"
        );
        bpa.shutdown().await;

        let flags = BlockFlags {
            delete_bundle_on_failure: true,
            ..BlockFlags::default()
        };
        let (inbound, _, keys) = protected_block(LOCAL, flags, true, true, true);
        let (bpa, saved_rx, deleted_rx, _) =
            keyed_node_receiving_in(keys, None, inbound, segments).await;
        bpa.shutdown().await;
        assert!(
            nothing_left_from(&saved_rx, &deleted_rx, &source),
            "{segments} segments: the bundle is deleted"
        );
    }
}

// A bundle from ipn:0.2.1, node 1's peer, asking for deletion reports, whose
// primary block its source signed, with the payload too where
// `with_payload`, in BIBs whose scope includes the primary block: the bundle
// and the signing key.
#[cfg(feature = "rfc9173")]
fn with_signed_primary(with_payload: bool) -> (Bytes, Key) {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let (built, data) = Builder::new(source.clone(), "ipn:0.3.99".parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let (sign_key, _) = generated_keys();
    let mut signing = Signer::new(&built, &data)
        .sign_block(
            0,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            source.clone(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("sign the primary block");
    if with_payload {
        signing = signing
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source,
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload");
    }
    (
        Bytes::from(signing.rebuild().expect("rebuild signed")),
        sign_key,
    )
}

// An announced primary block's BIB is verified and removed, and the primary
// is stored as received (RFC 9171 §4.3.1), with the CRC its source's signing
// left in place: so another operation that keeps the primary in its scope,
// as the payload's BIB does under RFC 9173's default scope, still verifies.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_primary_blocks_bib_is_removed_leaving_the_primary_as_received() {
    for with_payload in [false, true] {
        let (inbound, key) = with_signed_primary(with_payload);
        let received = parse(inbound.clone()).expect("the inbound bundle parses");
        let (bpa, saved_rx, _) =
            keyed_node_receiving(vec![key.clone()], accepting(&[Type::Primary]), inbound).await;
        let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
        assert_eq!(
            stored.bundle.blocks[&0].bib,
            BibCoverage::None,
            "the primary block's BIB is removed (payload signed: {with_payload})"
        );
        assert_ne!(stored.bundle.primary.crc_type, CrcType::None);
        assert_eq!(
            stored.bundle.blocks[&0].payload(&stored.data),
            received.bundle.blocks[&0].payload(&received.data),
            "the primary is stored as received (payload signed: {with_payload})"
        );
        let payload_signed = matches!(stored.bundle.blocks[&1].bib, BibCoverage::Some(_));
        assert_eq!(payload_signed, with_payload);
        common::verify_all_bibs(&stored.data, &KeySet::new(vec![key]))
            .expect("every remaining BIB verifies over the stored primary");
        bpa.shutdown().await;
    }
}

// A BIB on the primary block this node holds no key for is a verifier's
// skip, and the bundle is stored as received; announced, it is a failed
// acceptance, and the primary block, which has no flags and which the bundle
// cannot lose, deletes the bundle before anything is stored.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_announced_primary_block_without_its_key_deletes_the_bundle() {
    let (inbound, _) = with_signed_primary(false);
    let (bpa, saved_rx, _) = keyed_node_receiving(Vec::new(), None, inbound.clone()).await;
    stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
    bpa.shutdown().await;

    let (bpa, saved_rx, sent_rx) =
        keyed_node_receiving(Vec::new(), accepting(&[Type::Primary]), inbound).await;
    // Event-driven wait; the timeout only bounds a regression.
    let sent = tokio::time::timeout(tokio::time::Duration::from_secs(5), sent_rx.recv_async())
        .await
        .expect("timeout waiting for the deletion report")
        .expect("channel closed");
    let report = parse(sent).expect("the report parses");
    let body = report.bundle.blocks[&1]
        .payload(&report.data)
        .expect("the report's payload is resident");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("the payload is an admin record");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier: of what the node stored, only
    // its own report remains, never the deleted bundle.
    bpa.shutdown().await;
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    assert!(
        saved_rx.try_iter().all(|data| parse(data)
            .expect("a stored bundle parses")
            .bundle
            .primary
            .id
            .source
            != source),
        "the deleted bundle is never stored"
    );
}

// A block the ingress gate schedules for §E removal (here an unrecognised
// extension block flagged `delete_block_on_failure`) leaves the bundle
// before it is first stored: neither the stored bundle nor the one
// transmitted carries it, and the payload survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_removal_leaves_before_storage() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let (saved, saved_rx) = SavedBundles::new();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .bundle_storage(saved)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, &[remote])
        .await
        .unwrap();

    // Craft a bundle to the remote node carrying an unrecognised block
    // flagged delete_block_on_failure.
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let inbound = with_unknown_deletable_block(&build_bundle(&source, &dest, b"payload"));

    // The crafted bundle really carries the unrecognised block as it arrives.
    let Parsed { bundle: pre, .. } = parse(inbound.clone()).expect("crafted bundle parses");
    assert!(
        pre.blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the unknown block is present as ingressed"
    );

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "an unknown deletable block is accepted, not refused"
    );

    let stored = stored_from(&saved_rx, &source).await;
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the scheduled removal is applied before the bundle is stored"
    );
    assert!(
        stored.bundle.blocks.contains_key(&1),
        "the payload survives"
    );

    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the forwarded bundle")
    .expect("channel closed");

    let Parsed { bundle: fwd, .. } = parse(forwarded).expect("forwarded bundle parses");
    assert!(
        fwd.blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the transmitted bundle does not carry it"
    );
    assert!(fwd.blocks.contains_key(&1), "the payload survives");

    bpa.shutdown().await;
}

// A low-level (raw-bundle) service that captures each delivered bundle's
// wire bytes.
struct CapturingService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    delivered_tx: flume::Sender<Bytes>,
}

impl CapturingService {
    fn new() -> (Arc<Self>, flume::Receiver<Bytes>) {
        let (delivered_tx, rx) = flume::unbounded();
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                delivered_tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl services::Service for CapturingService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }
    async fn on_unregister(&self) {}
    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        let _ = self.delivered_tx.send(data);
        Ok(())
    }
    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// The stored record describes the stored bundle: once the scheduled removal
// is applied, the record's block map is the rebuilt one, without the removed
// block and with the payload where the stored bytes hold it. Nothing can
// forward the bundle, so it stays stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_removal_leaves_the_record_describing_the_stored_bundle() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let (saved, saved_rx) = SavedBundles::new();
    let records = Arc::new(MetadataMemStorage::new(None));
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .bundle_storage(saved)
        .metadata_storage(records.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let inbound = with_unknown_deletable_block(&build_bundle(&source, &dest, b"payload"));
    let id = parse(inbound.clone())
        .expect("crafted bundle parses")
        .bundle
        .primary
        .id;
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let stored = stored_from(&saved_rx, &source).await;
    let record = records
        .get(&id)
        .await
        .unwrap()
        .expect("the unforwardable bundle stays stored");
    assert!(
        record
            .bpv7
            .blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the record's block map is the rebuilt one"
    );
    assert_eq!(
        record.bpv7.blocks[&1].extent, stored.bundle.blocks[&1].extent,
        "the record locates the payload where the stored bytes hold it"
    );

    bpa.shutdown().await;
}

// The deliver-side twin of `scheduled_removal_leaves_before_storage`: a
// bundle addressed to a local raw-bundle service, carrying an unrecognised
// `delete_block_on_failure` block, is delivered without that block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_removal_is_absent_at_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (svc, delivered_rx) = CapturingService::new();
    let endpoint = bpa.register_service(Service::Ipn(7), svc).await.unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    // A bundle to the local service, carrying an unrecognised block (type
    // 999, block 2) flagged delete_block_on_failure.
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let base = build_bundle(&source, &endpoint, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64);
        a.emit(&2u64);
        a.emit(&0x10u64); // delete_block_on_failure
        a.emit(&0u64);
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let delivered = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        delivered_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the delivered bundle")
    .expect("channel closed");

    let Parsed { bundle: del, .. } = parse(delivered).expect("delivered bundle parses");
    assert!(
        del.blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the deferred removal is applied at the deliver door"
    );
    assert!(del.blocks.contains_key(&1), "the payload survives");

    bpa.shutdown().await;
}

// Splices an unrecognised block (type 999, block 2) flagged
// `delete_block_on_failure` between `base`'s primary and its first
// canonical block: the ingress gate schedules it for removal.
fn with_deletable_unknown(base: &Bytes) -> Bytes {
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x10u64); // flags: delete_block_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    Bytes::from(modified)
}

/// Records whether it was handed a block scheduled for removal (type 999),
/// then inserts a block of its own, which the editor numbers from the lowest
/// free block number — the removed block's, when the removal came first.
struct ScheduledBlockObserver {
    saw_scheduled: Arc<AtomicBool>,
}

impl Rewriter for ScheduledBlockObserver {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        if ctx
            .bundle()
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999))
        {
            self.saw_scheduled.store(true, Ordering::SeqCst);
        }
        // A refusal is a no-match, never a panic (a panicking filter aborts
        // the node); `assert_insert_survives` catches a refused insert.
        let _ = ctx.editor().insert(
            INSERTED,
            BlockFlags::default(),
            CrcType::None,
            emit(&42u64).0.into(),
        );
    }
}

// The bundle carries the Rewriter's insert and not the scheduled block.
fn assert_insert_survives(data: Bytes) {
    let out = parse(data).expect("the bundle parses");
    assert!(
        out.bundle
            .blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the scheduled removal is applied"
    );
    let inserted = out
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == INSERTED)
        .expect("the Rewriter's insert survives the removal");
    assert_eq!(inserted.payload(&out.data), Some(emit(&42u64).0.as_slice()));
}

/// The scheduled removals, applied before the bundle is stored, precede the
/// Egress chain: a Rewriter never sees a block scheduled for removal, and
/// the block it inserts travels, where a removal applied after it could
/// delete an insert that took the removed block's number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_removal_precedes_the_egress_rewriters() {
    let saw_scheduled = Arc::new(AtomicBool::new(false));
    let mut pack = FilterPack::new("test");
    pack.egress_rewriter(
        "observer",
        ScheduledBlockObserver {
            saw_scheduled: saw_scheduled.clone(),
        },
    );
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .add_filters(pack)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
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
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();

    let inbound = with_deletable_unknown(&build_bundle(
        &"ipn:0.9.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    ));
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the forwarded bundle")
    .expect("channel closed");

    assert!(
        !saw_scheduled.load(Ordering::SeqCst),
        "the Rewriter never sees the scheduled block"
    );
    assert_insert_survives(forwarded);

    bpa.shutdown().await;
}

/// The Deliver twin: the scheduled removals, applied before the bundle is
/// stored, precede the Deliver chain, so a Deliver Rewriter never sees a
/// block scheduled for removal, and the raw-bundle service receives its
/// insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_removal_precedes_the_deliver_chain() {
    let saw_scheduled = Arc::new(AtomicBool::new(false));
    let mut pack = FilterPack::new("test");
    pack.deliver_rewriter(
        "observer",
        ScheduledBlockObserver {
            saw_scheduled: saw_scheduled.clone(),
        },
    );
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .add_filters(pack)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (svc, delivered_rx) = CapturingService::new();
    let endpoint = bpa.register_service(Service::Ipn(7), svc).await.unwrap();
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let inbound = with_deletable_unknown(&build_bundle(
        &"ipn:0.9.1".parse().unwrap(),
        &endpoint,
        b"payload",
    ));
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let delivered = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        delivered_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the delivered bundle")
    .expect("channel closed");

    assert!(
        !saw_scheduled.load(Ordering::SeqCst),
        "the Rewriter never sees the scheduled block"
    );
    assert_insert_survives(delivered);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-02: Echo Round-Trip
// ---------------------------------------------------------------------------

/// A bundle dispatched via CLA to the echo service is reflected back
/// and forwarded out via the CLA with source/destination swapped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_round_trip() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    // Register echo service on service number 7
    let echo = EchoService::new();
    bpa.register_service(Service::Ipn(7), echo).await.unwrap();

    // Register CLA with a peer for the "remote" node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    // Build an inbound bundle: from remote node, to our echo service
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let echo_dest: Eid = "ipn:0.1.7".parse().unwrap();
    let mut inbound = build_bundle(&remote_source, &echo_dest, b"ping");

    // Dispatch it as if received from the CLA
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The echo service should reflect the bundle back:
    // source=ipn:0.1.7 (echo), dest=ipn:0.2.1 (remote)
    // BPA routes to CLA peer (ipn:0.2.*)
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for echo reply")
    .expect("Channel closed");

    // Parse and verify the echo reply (structural — no keys).
    let Parsed {
        bundle: parsed_bundle,
        ..
    } = parse(forwarded).expect("Failed to parse echo reply");

    // Source and destination should be swapped
    assert_eq!(
        parsed_bundle.primary.destination, remote_source,
        "Echo reply destination should be the original source"
    );
    assert_eq!(
        parsed_bundle.primary.id.source, echo_dest,
        "Echo reply source should be the echo service"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-12: Streamed service originate
// ---------------------------------------------------------------------------

// Register a service and a CLA peer; the returned (bpa, sink-holder, forwarded
// channel, service EID) drive ServiceSink::send directly with multi-segment streams.
async fn streamed_originate_setup() -> (Bpa, Arc<EchoService>, flume::Receiver<Bytes>, Eid) {
    streamed_originate_setup_with(None).await
}

// As `streamed_originate_setup`, storing bundles in `bundles` when given.
async fn streamed_originate_setup_with(
    bundles: Option<Arc<dyn BundleStorage>>,
) -> (Bpa, Arc<EchoService>, flume::Receiver<Bytes>, Eid) {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let mut builder = Bpa::builder().node_ids(node_ids);
    if let Some(bundles) = bundles {
        builder = builder.bundle_storage(bundles);
    }
    let bpa = builder.build().await.unwrap();
    bpa.start(false).await;

    // The service only captures its sink here; the test drives the sink itself.
    let svc = EchoService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    (bpa, svc, forwarded_rx, "ipn:0.1.7".parse().unwrap())
}

/// A bundle streamed through `ServiceSink::send` in several segments
/// originates identically to a whole-buffer `send`: the returned id matches
/// the built bundle, and the bundle is forwarded to the CLA peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_originate_forwards() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"streamed-originate");

    // Split into two Next segments and a Final; the channel is sized to hold
    // them all so the producer side completes before the sink pulls.
    let third = data.len() / 3;
    let (tx, mut rx) = hardy_async::channel::bounded(4);
    tx.send(Segment::Next(data.slice(..third))).await.unwrap();
    tx.send(Segment::Next(data.slice(third..2 * third)))
        .await
        .unwrap();
    tx.send(Segment::Final(data.slice(2 * third..)))
        .await
        .unwrap();
    drop(tx);

    let id = svc
        .sink
        .get()
        .unwrap()
        .send(&mut rx)
        .await
        .expect("streamed send failed");
    assert_eq!(id.source, source_eid);

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id, id);
    assert_eq!(parsed.bundle.primary.destination, dest);

    bpa.shutdown().await;
}

/// Dropping the producer before `Final` cancels the send: the caller gets
/// `StreamCancelled` and nothing enters custody (nothing is forwarded).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_cancel_stores_nothing() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"cancelled");

    let (tx, mut rx) = hardy_async::channel::bounded(4);
    tx.send(Segment::Next(data.slice(..data.len() / 2)))
        .await
        .unwrap();
    drop(tx); // no Final — the producer aborts

    let result = svc.sink.get().unwrap().send(&mut rx).await;
    assert!(matches!(
        result,
        Err(hardy_bpa::services::Error::StreamCancelled)
    ));

    // Nothing was originated: no forward may arrive.
    assert!(
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            forwarded_rx.recv_async(),
        )
        .await
        .is_err(),
        "cancelled stream must not originate a bundle"
    );

    bpa.shutdown().await;
}

/// Unregistering a service wakes its in-flight sends immediately: a consumer
/// parked behind a stalled producer fails with `StreamCancelled` the moment
/// the registration dies, without waiting for the producer's next segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_unregister_cancels_parked_send() {
    let (bpa, svc, _forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"parked");

    // One segment then a stall — the sender stays alive throughout, so only
    // registration teardown can end the stream.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    hardy_async::channel::Sender::send(&tx, Segment::Next(data.slice(..data.len() / 2)))
        .await
        .unwrap();

    let parked = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.sink.get().unwrap().send(&mut rx).await })
    };

    // Let the consumer enter the stream and park on the second pull.
    // Known test-guide deviation (timed quiesce): scheduled for the
    // dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    svc.sink.get().unwrap().unregister().await;

    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), parked)
        .await
        .expect("parked send was not woken by unregistration")
        .expect("task panicked");
    assert!(matches!(
        result,
        Err(hardy_bpa::services::Error::StreamCancelled)
    ));
    drop(tx);

    bpa.shutdown().await;
}

/// The raw service door carries a bundle as its service built it: a block
/// CLA ingress would remove is the receiving nodes' to handle by its flags,
/// so it is stored and forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originate_raw_carries_the_bundle_as_built() {
    let (saved, saved_rx) = SavedBundles::new();
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup_with(Some(saved)).await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = with_unknown_deletable_block(&build_bundle(&source_eid, &dest, b"originated"));
    originate_raw(&svc, data)
        .await
        .expect("origination admits the bundle");

    let stored = stored_from(&saved_rx, &source_eid).await;
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the bundle is stored as built"
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the forwarded bundle")
    .expect("channel closed");
    let forwarded = parse(forwarded).expect("the forwarded bundle parses");
    assert!(
        forwarded
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the bundle is forwarded as built"
    );

    bpa.shutdown().await;
}

/// The source-spoof check holds on the streamed path: a bundle whose source
/// is not the registered service endpoint is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_send_rejects_spoofed_source() {
    let (bpa, svc, _forwarded_rx, _source_eid) = streamed_originate_setup().await;

    let spoofed: Eid = "ipn:0.1.99".parse().unwrap();
    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&spoofed, &dest, b"spoofed");

    let (tx, mut rx) = hardy_async::channel::bounded(1);
    tx.send(Segment::Final(data)).await.unwrap();
    drop(tx);

    let result = svc.sink.get().unwrap().send(&mut rx).await;
    assert!(matches!(
        result,
        Err(hardy_bpa::services::Error::InvalidDestination(_))
    ));

    bpa.shutdown().await;
}

// Sends `data` through the raw service door, returning its verdict.
async fn originate_raw(svc: &EchoService, data: Bytes) -> services::Result<Id> {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    tx.send(Segment::Final(data)).await.unwrap();
    drop(tx);
    svc.sink.get().unwrap().send(&mut rx).await
}

/// Origination refuses a Hop Count block a BCB covers: this node's egress
/// rewrites the block at once, so no security operation on it could reach
/// its acceptor.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_send_refuses_a_protected_hop_count() {
    let (bpa, svc, _forwarded_rx, source_eid) = streamed_originate_setup().await;

    // Immaterial key value: generated per the no-literal-keys rule.
    let mut key_bytes = vec![0u8; 32];
    SysRng.try_fill_bytes(&mut key_bytes).unwrap();
    let key = Key {
        key_type: KeyType::octet_sequence(key_bytes),
        key_algorithm: None,
        enc_algorithm: Some(EncAlgorithm::A256GCM),
        operations: Some([Operation::Encrypt].into_iter().collect()),
        id: None,
        key_use: None,
    };
    let (built, data) = Builder::new(source_eid.clone(), "ipn:0.2.1".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 0,
        })
        .with_payload(b"undecryptable hop count".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let hop_count = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::HopCount)
        .expect("the builder emitted the Hop Count block")
        .0;
    let encrypted = parse(Bytes::from(
        Encryptor::new(&built, &data)
            .encrypt_block(
                hop_count,
                EncryptContext::AES_GCM(Default::default()),
                source_eid,
                &key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the Hop Count block")
            .rebuild()
            .expect("rebuild the encrypted bundle"),
    ))
    .unwrap();

    assert!(matches!(
        originate_raw(&svc, encrypted.data).await,
        Err(services::Error::ProtectedPerHopBlock(n)) if n == hop_count
    ));

    bpa.shutdown().await;
}

/// Origination refuses a Previous Node block: an originated bundle has no
/// previous node (RFC 9171 §4.4.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_send_refuses_a_previous_node() {
    let (bpa, svc, _forwarded_rx, source_eid) = streamed_originate_setup().await;

    let (_, data) = Builder::new(source_eid, "ipn:0.2.1".parse().unwrap())
        .add_extension_block(Type::PreviousNode)
        .unwrap()
        .build(emit(&"ipn:0.9.0".parse::<Eid>().unwrap()).0.into())
        .with_payload(b"a previous node".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();

    assert!(matches!(
        originate_raw(&svc, Bytes::from(data)).await,
        Err(services::Error::PreviousNodeBlock)
    ));

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-03: Local Delivery
// ---------------------------------------------------------------------------

/// A bundle dispatched via CLA addressed to a local application is
/// delivered to that application's on_deliver callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    // Register an application on service number 42
    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    // Register CLA (needed for dispatch)
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    // Build an inbound bundle addressed to our local application
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let mut inbound = build_bundle(&remote_source, &local_dest, b"Hello local");

    // Dispatch via CLA
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Application should receive the payload
    let (source, payload) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for local delivery")
            .expect("Channel closed");

    assert_eq!(source, remote_source, "Delivered source should match");
    assert_eq!(
        payload.as_ref(),
        b"Hello local",
        "Delivered payload should match"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Reception report reason codes (RFC 9172 §7.1)
// ---------------------------------------------------------------------------

// Splice a BCB with an unrecognised security context (id 99) targeting the
// payload into `data` as block number 2, flagged must-replicate (required
// for a payload target) + report-on-failure.
fn splice_unrecognised_bcb(data: &[u8]) -> Bytes {
    splice_unrecognised_security_block(data, 12, 2, 1, 0x03)
}

// Splice a security block of type `block_type` (11, a BIB, or 12, a BCB),
// numbered `number`, with an unrecognised security context (id 99), over the
// block numbered `target`, into `data`, with the block flags `flags`.
fn splice_unrecognised_security_block(
    data: &[u8],
    block_type: u64,
    number: u64,
    target: u64,
    flags: u64,
) -> Bytes {
    // ASB CBOR sequence: targets [target], context id 99, context flags 0
    // (no parameters), source EID, then one result list per target.
    let result_val = [0x41u8, 0xAA]; // result value: bytes(0xAA)
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let mut asb = emit(&[target]).0;
    asb.extend(emit(&99u64).0);
    asb.extend(emit(&0u64).0);
    asb.extend(emit(&source).0);
    asb.extend(emit_array(Some(1), |results| {
        results.emit_array(Some(1), |target_results| {
            target_results.emit(&(1u64, Raw(&result_val)));
        });
    }));

    let bcb_block = emit_array(Some(5), |a| {
        a.emit(&block_type);
        a.emit(&number);
        a.emit(&flags);
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&asb));
    });

    assert_eq!(data[0], 0x9F, "Bundle should start with indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("Should skip primary block");
    let insert_pos = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + bcb_block.len());
    modified.extend_from_slice(&data[..insert_pos]);
    modified.extend_from_slice(&bcb_block);
    modified.extend_from_slice(&data[insert_pos..]);
    modified.into()
}

// A transit bundle carrying a BCB this node cannot understand (unrecognised
// security context, `report_on_failure` set) is forwarded with it intact: an
// unsupported operation's flags are its acceptor's, so a relay neither acts
// on them nor reports it, and the requested reception report carries no
// reason. The bundle is addressed to a remote node so it forwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_leaves_an_unknown_security_operation_unreported() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // Register CLA with a peer for the remote node (ipn:0.2) — the route for
    // both the forwarded bundle and the reception report (report-to defaults
    // to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    // Inbound transit bundle requesting a reception report.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let (_, data) = Builder::new(remote_source.clone(), dest.clone())
        .with_flags(Flags {
            receipt_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = splice_unrecognised_bcb(&data);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Two bundles come back out of the CLA in either order: the reception
    // report (to ipn:0.2.1) and the forwarded original (to ipn:0.2.99).
    let mut report = None;
    let mut original = None;
    for _ in 0..2 {
        // Event-driven wait; the timeout only bounds a regression.
        let forwarded = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for forwarded bundle")
        .expect("Channel closed");
        let parsed = parse(forwarded).expect("Failed to parse forwarded");
        if parsed.bundle.primary.flags.is_admin_record {
            report = Some(parsed);
        } else {
            original = Some(parsed);
        }
    }

    // The original is forwarded intact, BCB and all (an operation we cannot
    // understand is left for a downstream security acceptor).
    let original = original.expect("Original bundle should be forwarded");
    assert_eq!(original.bundle.primary.destination, dest);
    assert!(
        original
            .bundle
            .blocks
            .values()
            .any(|b| matches!(b.block_type, Type::BlockSecurity)),
        "Forwarded bundle should still carry the BCB"
    );

    // The reception report goes to the source, and names no operation.
    let report = report.expect("Reception report should be emitted");
    assert_eq!(report.bundle.primary.destination, remote_source);
    let body = report
        .bundle
        .blocks
        .get(&1)
        .expect("Report has a payload block")
        .payload(&report.data)
        .expect("Report payload in bundle");
    let record = hardy_cbor::decode::parse::<AdministrativeRecord>(body)
        .expect("Report payload is an administrative record");
    let AdministrativeRecord::BundleStatusReport(status) = record;
    assert_eq!(status.reason, ReasonCode::NoAdditionalInformation);
    assert!(status.received.is_some(), "Reception assertion present");
    assert_eq!(status.bundle_id.source, remote_source);

    bpa.shutdown().await;
}

// At the destination, a BCB in an unrecognised security context over the
// payload leaves a payload this node cannot accept: a bundle cannot lose its
// payload (RFC 9171 §4.1), so the bundle is deleted, "Block unsupported",
// whatever the payload's flags, here none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destination_deletes_a_bundle_whose_payload_it_cannot_accept() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let (_, data) = Builder::new(remote_source.clone(), "ipn:0.1.99".parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .add_extension_block(Type::Payload)
        .unwrap()
        .with_flags(BlockFlags::default())
        .build(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = splice_unrecognised_bcb(&data);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let sent = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the deletion report")
    .expect("Channel closed");
    let report = parse(sent).expect("the report parses");
    let body = report.bundle.blocks[&1]
        .payload(&report.data)
        .expect("the report's payload is resident");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("the payload is an admin record");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);
    bpa.shutdown().await;
}

// At the bundle's destination, the acceptor of every operation in it (RFC
// 9172 §1.4), a BIB in an unrecognised security context over an extension
// block is one this node cannot process: flagged `report_on_failure`, it
// demands the reception report, with the RFC 9172 `UnknownSecurityOperation`
// reason; the block it covers, whose operation this node cannot accept, is
// discarded, and the bundle is kept for its service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destination_reports_an_unknown_security_operation_it_accepts() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let (saved, saved_rx) = SavedBundles::new();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .bundle_storage(saved)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    // A bundle for a local service, not yet registered, with an extension
    // block (type 199, block 2) under a BIB (block 3) in context 99, flagged
    // report-on-failure.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let (_, data) = Builder::new(remote_source.clone(), "ipn:0.1.99".parse().unwrap())
        .add_extension_block(Type::Unrecognised(199))
        .unwrap()
        .build(b"covered".as_slice().into())
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = splice_unrecognised_security_block(&data, 11, 3, 2, 0x02);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let sent = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the reception report")
    .expect("Channel closed");
    let report = parse(sent).expect("the report parses");
    let body = report.bundle.blocks[&1]
        .payload(&report.data)
        .expect("the report's payload is resident");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("the payload is an admin record");
    assert_eq!(status.reason, ReasonCode::UnknownSecurityOperation);
    assert!(status.deleted.is_none(), "the bundle is kept");

    let stored = stored_from(&saved_rx, &remote_source).await;
    assert!(
        !stored.bundle.blocks.contains_key(&2),
        "the covered block is discarded"
    );
    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-12: Streamed CLA ingress
// ---------------------------------------------------------------------------

/// A bundle dispatched through `Sink::dispatch` in several segments
/// ingresses identically to the whole-buffer `dispatch`: the reassembled
/// bundle reaches its local application.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_streamed_ingress_delivers() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"streamed ingress");

    // Three segments through a bounded channel with a spawned producer, so
    // the pull side is genuinely driving.
    let third = inbound.len() / 3;
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let segments = vec![
        Segment::Next(inbound.slice(..third)),
        Segment::Next(inbound.slice(third..2 * third)),
        Segment::Final(inbound.slice(2 * third..)),
    ];
    let producer = tokio::spawn(async move {
        for segment in segments {
            hardy_async::channel::Sender::send(&tx, segment)
                .await
                .unwrap();
        }
    });

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut rx)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    producer.await.unwrap();

    let (source, payload) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for streamed delivery")
            .expect("Channel closed");
    assert_eq!(source, remote_source);
    assert_eq!(payload.as_ref(), b"streamed ingress");

    bpa.shutdown().await;
}

/// Unregistering a CLA wakes its in-flight streams immediately: a consumer
/// parked behind a stalled producer fails with `StreamCancelled` the moment
/// the registration dies, without waiting for the producer's next segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_unregister_cancels_parked_stream() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    // A producer that sends one segment then stalls — the sender stays
    // alive throughout, so only registration teardown can end the stream.
    // The segment is a genuine bundle prefix: the ingress gate parses
    // eagerly, so arbitrary bytes would be rejected structurally instead
    // of leaving the consumer parked on the next pull.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"parked");
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    hardy_async::channel::Sender::send(&tx, Segment::Next(inbound.slice(..inbound.len() / 2)))
        .await
        .unwrap();

    let parked = {
        let cla = cla.clone();
        tokio::spawn(async move { cla.sink.get().unwrap().dispatch(None, None, &mut rx).await })
    };

    // Let the consumer enter the stream and park on the second pull.
    // Known test-guide deviation (timed quiesce): scheduled for the
    // dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    cla.sink.get().unwrap().unregister().await;

    // The parked pull must fail promptly — the assertion is event-driven;
    // the timeout only bounds a regression. The teardown is reported as the
    // dead registration, not a per-bundle refusal.
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), parked)
        .await
        .expect("parked stream was not woken by unregistration")
        .expect("task panicked");
    assert!(matches!(result, Err(hardy_bpa::cla::Error::Disconnected)));
    drop(tx);

    bpa.shutdown().await;
}

/// A producer that dies before `Final` is a truncation: the sink refuses
/// acceptance (so a CLA withholds its transfer ack) and nothing is
/// delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_streamed_ingress_truncation_is_refused() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"truncated");

    let (tx, mut rx) = hardy_async::channel::bounded(4);
    hardy_async::channel::Sender::send(&tx, Segment::Next(inbound.slice(..inbound.len() / 2)))
        .await
        .unwrap();
    drop(tx); // no Final

    let result = cla.sink.get().unwrap().dispatch(None, None, &mut rx).await;
    assert!(matches!(result, Ok(cla::Acceptance::Refused)));

    // Known test-guide deviation (quiet-window absence assert):
    // scheduled for the dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_millis(500), app_rx.recv_async())
            .await
            .is_err(),
        "truncated stream must not deliver"
    );

    bpa.shutdown().await;
}

// A CLA-door node with an application on ipn:0.1.42, for the abandoned
// transfer tests: the BPA, the application's delivery receiver, and the CLA.
async fn abandoned_transfer_setup() -> (
    Bpa,
    Arc<TestApp>,
    flume::Receiver<(Eid, Bytes)>,
    Arc<PipelineCla>,
) {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    (bpa, app, app_rx, cla)
}

// Sends `segments` (no `Final` among them), drops the producer, and returns
// the sink's verdict.
async fn dispatch_abandoned(cla: &PipelineCla, segments: Vec<Segment>) -> cla::Acceptance {
    let (tx, mut rx) = hardy_async::channel::bounded(segments.len());
    for segment in segments {
        hardy_async::channel::Sender::send(&tx, segment)
            .await
            .unwrap();
    }
    drop(tx); // no Final

    cla.sink
        .get()
        .unwrap()
        .dispatch(None, None, &mut rx)
        .await
        .unwrap()
}

/// A whole bundle sent in `Segment::Next` whose producer then dies before
/// `Final` is an abandoned transfer: the sink refuses it (so a CLA withholds
/// its transfer ack) and nothing is delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_complete_at_head_without_final_is_refused() {
    let (bpa, _app, app_rx, cla) = abandoned_transfer_setup().await;

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"abandoned");

    assert_eq!(
        dispatch_abandoned(&cla, vec![Segment::Next(inbound)]).await,
        cla::Acceptance::Refused
    );

    // The completed shutdown is the barrier proving nothing was delivered.
    bpa.shutdown().await;
    assert!(app_rx.is_empty(), "an abandoned transfer must not deliver");
}

/// The same for a bundle segmented past its outer break: the
/// producer dies after the last byte but before `Final`, and the sink refuses
/// the transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_segmented_without_final_is_refused() {
    let (bpa, _app, app_rx, cla) = abandoned_transfer_setup().await;

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, &vec![0xA5_u8; 20_000]);

    // Every byte arrives, in 1000-byte `Next` segments (forces Partial).
    let segments = inbound
        .chunks(1000)
        .map(|chunk| Segment::Next(Bytes::copy_from_slice(chunk)))
        .collect();
    assert_eq!(
        dispatch_abandoned(&cla, segments).await,
        cla::Acceptance::Refused
    );

    // The completed shutdown is the barrier proving nothing was delivered.
    bpa.shutdown().await;
    assert!(app_rx.is_empty(), "an abandoned transfer must not deliver");
}

// ---------------------------------------------------------------------------
// Streamed ingress: a CLA delivers a bundle split across many segments, its
// payload arriving after its header — exercising the `Partial` path
// end-to-end, the payload drained by the `ValidatingReceiver`.
// ---------------------------------------------------------------------------

// A `Receiver` that yields a fixed sequence of segments then reports the
// producer is gone — mimics a CLA reassembling a transfer into segments.
struct SegmentReceiver {
    segments: Mutex<VecDeque<cla::Segment>>,
}

impl SegmentReceiver {
    // Split `data` into `chunk`-byte segments: `Next` for all but the last,
    // which is `Final`.
    fn new(data: &[u8], chunk: usize) -> Self {
        let mut segments = VecDeque::new();
        let mut off = 0;
        while off < data.len() {
            let end = (off + chunk).min(data.len());
            let piece = Bytes::copy_from_slice(&data[off..end]);
            segments.push_back(if end == data.len() {
                cla::Segment::Final(piece)
            } else {
                cla::Segment::Next(piece)
            });
            off = end;
        }
        Self {
            segments: Mutex::new(segments),
        }
    }

    // Segments not yet pulled by the parser. Non-zero after dispatch means the
    // payload tail was never drained.
    fn remaining(&self) -> usize {
        self.segments.lock().unwrap().len()
    }
}

#[async_trait]
impl Receiver<cla::Segment> for SegmentReceiver {
    async fn recv(&mut self) -> Result<cla::Segment, hardy_bpa::stream::RecvError> {
        self.segments
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(hardy_bpa::stream::RecvError)
    }
}

// A segment source that yields a fixed prefix of a bundle's segments and
// then parks forever — a transfer whose producer never completes. A
// consumer that returns while this source is parked provably did not wait
// for the payload.
struct ParkedReceiver {
    segments: VecDeque<cla::Segment>,
}

impl ParkedReceiver {
    // The first `count` `chunk`-byte segments of `data`, all `Next` — the
    // stream never completes.
    fn new(data: &[u8], chunk: usize, count: usize) -> Self {
        Self {
            segments: data
                .chunks(chunk)
                .take(count)
                .map(|c| cla::Segment::Next(Bytes::copy_from_slice(c)))
                .collect(),
        }
    }
}

#[async_trait]
impl Receiver<cla::Segment> for ParkedReceiver {
    async fn recv(&mut self) -> Result<cla::Segment, hardy_bpa::stream::RecvError> {
        match self.segments.pop_front() {
            Some(seg) => Ok(seg),
            None => futures::future::pending().await,
        }
    }
}

// An inbound bundle with a 20 KB payload, delivered in 1000-byte segments, is
// reassembled and delivered intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn segmented_payload_local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];
    let inbound = build_bundle(&remote_source, &local_dest, &payload);

    // Deliver the bundle as a stream of 1000-byte segments (forces Partial).
    let mut stream = SegmentReceiver::new(&inbound, 1000);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut stream)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let (source, delivered) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for streamed local delivery")
            .expect("Channel closed");
    assert_eq!(source, remote_source, "Delivered source should match");
    assert_eq!(
        delivered.as_ref(),
        payload.as_slice(),
        "Full streamed payload should be delivered intact"
    );

    bpa.shutdown().await;
}

// A replayed arrival is settled at the ingress gate: the first copy was
// committed (and, once delivered, tombstoned), so the replay's duplicate
// probe fires while its payload source is still parked — the transfer is
// acked and dropped without spooling a byte. A consumer that returns while
// the source is parked provably did not wait for the payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_arrival_never_awaits_the_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];
    let inbound = build_bundle(&remote_source, &local_dest, &payload);

    // First copy: delivered whole, so the record commits and its id enters
    // the gates' recently-committed cache (delivery tombstones the record,
    // but the probe answers from the cached id, not the record's state).
    let mut stream = SegmentReceiver::new(&inbound, 1000);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut stream)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
        .await
        .expect("Timeout waiting for first delivery")
        .expect("Channel closed");

    // Replay: headers resident, payload parked forever. The gate's duplicate
    // probe must settle the transfer — acked, dropped — without the payload.
    // The timeout only bounds a regression: a door that waits for the
    // payload parks forever.
    let mut replay = ParkedReceiver::new(&inbound, 1000, 3);
    let acceptance = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        cla.sink.get().unwrap().dispatch(None, None, &mut replay),
    )
    .await
    .expect("Timeout: the duplicate arrival awaited its parked payload")
    .unwrap();
    assert_eq!(
        acceptance,
        cla::Acceptance::Accepted,
        "a committed duplicate is acked so the peer stops resending"
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(app_rx.is_empty(), "the duplicate must not deliver");
}

// A bundle whose creation time + lifetime is already in the past — expired on
// arrival. Sent in segments, its payload streams as `Partial`.
fn build_expired_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let past = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let timestamp = CreationTimestamp::from_parts(Some(DtnTime::saturating_from(past)), 1);
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_lifetime(Duration::from_secs(60))
        .with_payload(Cow::Borrowed(payload))
        .build(timestamp)
        .expect("Failed to build expired bundle");
    Bytes::from(data)
}

// A bundle carrying a Hop Count block whose count already exceeds its limit.
// Sent in segments, its payload streams as `Partial`.
fn build_hop_exhausted_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let hop = HopInfo {
        limit: NonZeroU8::new(1).unwrap(),
        count: 2,
    };
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_hop_count(&hop)
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build hop-exhausted bundle");
    Bytes::from(data)
}

// The §5.4 early-reject gate: a streamed bundle that fails a header-only check
// (lifetime, hop count) is dropped *before* its payload tail is drained, so the
// CLA never has to spool a gigantic invalid payload. Asserted by counting the
// segments left un-pulled in the `SegmentReceiver`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn segmented_gate_drops_before_draining_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let sink = || cla.sink.get().unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];

    // Expired — gated on lifetime; the payload tail must stay un-pulled.
    let expired = build_expired_bundle(&remote_source, &local_dest, &payload);
    let mut stream = SegmentReceiver::new(&expired, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert!(
        stream.remaining() > 0,
        "expired bundle must be dropped before the payload tail is drained"
    );

    // Hop-exhausted — gated on hop count; same expectation.
    let hopped = build_hop_exhausted_bundle(&remote_source, &local_dest, &payload);
    let mut stream = SegmentReceiver::new(&hopped, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert!(
        stream.remaining() > 0,
        "hop-exhausted bundle must be dropped before the payload tail is drained"
    );

    // Control: a valid bundle of the same size passes the gate, drains fully,
    // and delivers — proving the un-pulled segments above are the gate at work,
    // not a stalled stream.
    let valid = build_bundle(&remote_source, &local_dest, &payload);
    let mut stream = SegmentReceiver::new(&valid, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert_eq!(
        stream.remaining(),
        0,
        "a valid bundle drains its whole payload"
    );

    let (_src, delivered) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for the valid control bundle")
            .expect("Channel closed");
    assert_eq!(delivered.as_ref(), payload.as_slice());
    assert!(
        app_rx.is_empty(),
        "neither gated bundle should have been delivered"
    );

    bpa.shutdown().await;
}

// The gate's reporting split: a hop-exhausted arrival with the report flags
// set emits the combined §5.6/§5.10 status report — one §6.1.1 record
// asserting both reception and deletion, citing `HopLimitExceeded` — while
// an already-expired arrival with the same flags emits nothing at all
// (anti-amplification: it is treated as if it never arrived). Swapping the
// two gate branches fails both halves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_reports_hop_exhaustion_but_not_expiry() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A CLA with a peer for the remote node — the route for the reports
    // (report-to defaults to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let report_flags = Flags {
        receipt_report_requested: true,
        delete_report_requested: true,
        ..Default::default()
    };

    // Hop-exhausted, report-requesting transit bundle.
    let (_, data) = Builder::new(remote_source.clone(), dest.clone())
        .with_flags(report_flags.clone())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(1).unwrap(),
            count: 2,
        })
        .with_payload(Cow::Borrowed(b"opaque".as_slice()))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The single combined status report comes out of the CLA — reception and
    // deletion asserted in one record; the bundle itself must not be
    // forwarded. Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only status reports may leave the node for a gated bundle"
    );
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::HopLimitExceeded);

    // An already-expired arrival with the same report flags: total silence.
    let past = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let timestamp = CreationTimestamp::from_parts(Some(DtnTime::saturating_from(past)), 1);
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(report_flags)
        .with_lifetime(Duration::from_secs(60))
        .with_payload(Cow::Borrowed(b"opaque".as_slice()))
        .build(timestamp)
        .unwrap();
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "an expired-at-arrival bundle must produce no reports at all"
    );
}

// A complete-but-invalid streamed payload — a CRC mismatch the drain's
// `ValidatingReceiver` detects after the header pass admitted the bundle — is
// accepted, terminated, and reported exactly like a gate drop: the §5.6/§5.10
// reception + deletion pair, the deletion citing `BlockUnintelligible`. The
// transfer is accepted, never refused: the content cannot become valid by
// resending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_failure_reports_reception_then_deletion() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A CLA with a peer for the remote node — the route for the reports
    // (report-to defaults to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // A bundle fed in CLA-sized chunks: its payload block is incomplete when
    // its header parses, so the bundle takes the `Partial` route and the
    // payload streams through the validating drain. One
    // corrupt byte deep in the payload body fails the payload CRC there —
    // after the header pass admitted the bundle.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Owned(vec![0xA5_u8; 50_000]))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut data = data.into_vec();
    let corrupt_at = data.len() - 100; // inside the payload body, before its CRC field
    data[corrupt_at] ^= 0xFF;

    let mut stream = SegmentReceiver::new(&data, 1000);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut stream)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-corrupt transfer is accepted and terminated, never refused"
    );

    // The single combined status report comes out of the CLA — reception and
    // deletion asserted in one record; the bundle itself must not be
    // forwarded. Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only status reports may leave the node for a drain-dropped bundle"
    );
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnintelligible);

    // The completed shutdown is the barrier proving nothing else left the
    // node — one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A keyed header-pass failure with a recoverable bundle id — here an
// unrecognised extension block flagged `delete_bundle_on_failure`, fatal at
// the §A classify — is reported exactly like a gate or drain drop: one
// status report asserting both reception and deletion, citing
// `BlockUnsupported` (§5.6 Step 4's delete-bundle case). The transfer is
// accepted: the content cannot become valid by resending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_failure_reports_reception_then_deletion() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A CLA with a peer for the remote node — the route for the reports
    // (report-to defaults to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // A report-requesting bundle carrying an unrecognised block (type 999,
    // block 2) flagged delete_bundle_on_failure, spliced between the primary
    // and the payload.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x04u64); // flags: delete_bundle_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(data[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown.len());
    modified.extend_from_slice(&data[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&data[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-unsupported bundle is accepted and terminated, never refused"
    );

    // The single combined status report comes out of the CLA — reception and
    // deletion asserted in one record; the bundle itself must not be
    // forwarded. Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only status reports may leave the node for a rejected bundle"
    );
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier proving nothing else left the
    // node — one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// §5.6 Step 4 reports, then deletes: an unrecognised block flagged both
// `report_on_failure` and `delete_bundle_on_failure`, on a bundle requesting
// no status reports at bundle level, still raises its reception report
// citing `BlockUnsupported`. The report carries no deletion assertion (none
// was requested); the deletion the second flag orders shows as the bundle
// never leaving the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step4_block_flag_report_survives_delete_bundle() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // No bundle-level report flags; unrecognised block (type 999, block 2)
    // flagged report_on_failure and delete_bundle_on_failure, spliced after
    // the primary.
    let base = build_bundle(&remote_source, &dest, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x06u64); // flags: report_on_failure | delete_bundle_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-unsupported bundle is accepted and terminated, never refused"
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the block-flag reception report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only the status report may leave the node"
    );
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_none(), "no deletion report was requested");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier proving nothing else left.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A rejected bundle that requested only reception reporting (no deletion
// reports) still carries the §5.6 Step-4 facts on its reception-only status
// report: an unrecognised `report_on_failure` block sets the reception
// reason to `BlockUnsupported`, and a hop-exhausted gate drop must not
// squash it to `NoAdditionalInformation` — with no deletion asserted, the
// record's one reason slot belongs to the reception assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reception_only_reject_carries_step4_reason() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // Hop-exhausted, reception-report-only bundle carrying an unrecognised
    // block (type 999, block 3 — the builder's Hop Count block takes 2)
    // flagged report_on_failure.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            ..Default::default()
        })
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(1).unwrap(),
            count: 2,
        })
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&3u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(data[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown.len());
    modified.extend_from_slice(&data[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&data[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // One reception-only report: received asserted with the Step-4 reason,
    // no deletion asserted (deletion reports were not requested).
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(parsed.bundle.primary.flags.is_admin_record);
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert!(status.received.is_some(), "reception asserted");
    assert!(
        status.deleted.is_none(),
        "no deletion asserted without the request flag"
    );
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier proving nothing else left the
    // node — one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// §5.6 Step 4's block-flag-alone trigger: a bundle requesting NO status
// reports at bundle level, carrying an unrecognised block flagged
// `report_on_failure`, still generates a reception report citing
// `BlockUnsupported` — the block's own flag is the request. The bundle
// itself is forwarded intact, unrecognised block and all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step4_block_flag_alone_forces_reception_report() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // No bundle-level report flags at all; unrecognised block (type 999,
    // block 2) flagged report_on_failure, spliced after the primary.
    let base = build_bundle(&remote_source, &dest, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Two bundles leave the node in either order: the forced reception
    // report (to the source) and the forwarded original.
    let mut report = None;
    let mut original = None;
    for _ in 0..2 {
        // Event-driven wait; the timeout only bounds a regression.
        let forwarded = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for a forwarded bundle")
        .expect("Channel closed");
        let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
        if parsed.bundle.primary.flags.is_admin_record {
            report = Some(parsed);
        } else {
            original = Some(parsed);
        }
    }

    // The original forwards intact — a report flag never removes the block.
    let original = original.expect("original bundle forwarded");
    assert!(
        original
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the unrecognised block rides on unchanged"
    );

    // The report asserts reception only, citing the Step-4 reason.
    let report = report.expect("block-demanded reception report emitted");
    assert_eq!(report.bundle.primary.destination, remote_source);
    let body = report
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&report.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_none(), "nothing was deleted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    bpa.shutdown().await;
}

// The null-endpoint carve-out for the forced report: the same
// block-demanded bundle whose report-to is `dtn:none` produces no report —
// there is nowhere to send it. (A bundle with no bundle-level report flags
// may lawfully carry a null report-to; only the §4.2.3 flag combinations
// are parse-rejected.) The bundle still forwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step4_forced_report_suppressed_for_null_report_to() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    let (_, data) = Builder::new(remote_source.clone(), dest.clone())
        .with_report_to("dtn:none".parse().unwrap())
        .with_payload(Cow::Borrowed(b"payload"))
        .build(CreationTimestamp::now())
        .unwrap();
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(data[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown.len());
    modified.extend_from_slice(&data[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&data[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Only the original leaves the node.
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the forwarded bundle")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        !parsed.bundle.primary.flags.is_admin_record,
        "no report may be addressed to the null endpoint"
    );
    assert_eq!(parsed.bundle.primary.destination, dest);

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one bundle leaves the node"
    );
}

// A transfer that delivers its `Final` with the bundle short is complete: the
// bundle is invalid, so it is accepted and dropped, never refused (a custodial
// peer would resend the same bytes until they expire), and its headers parsed,
// so the drop is reported. A single `Final` carrying a bundle whose payload is
// cut short is never handed to the payload drain, whose stream is exhausted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_bundle_in_one_final_is_reported() {
    assert_short_final_is_reported(short_bundle(), None).await;
}

// The same short bundle in 1000-byte segments: the header pass reaches
// `Partial` on a `Next`, so the short payload ends in the payload drain's
// `Final`, which reports it the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_bundle_in_segments_is_reported() {
    assert_short_final_is_reported(short_bundle(), Some(1000)).await;
}

// A bundle short by less than one parser chunk, in a single `Final`, is
// reported too: the parser does not wait for bytes that will never come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_final_short_within_one_parser_chunk_is_reported() {
    let full = reporting_bundle(10_000);
    assert_short_final_is_reported(full.slice(..full.len() - 100), None).await;
}

// The same short bytes without their `Final` are an abandoned transfer, not a
// reception: refused, so the peer may resend, and never reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_short_transfer_is_refused_and_unreported() {
    let (bpa, _app, app_rx, cla, forwarded_rx) = short_final_setup().await;
    let segments = short_bundle()
        .chunks(1000)
        .map(|chunk| Segment::Next(Bytes::copy_from_slice(chunk)))
        .collect();
    assert_eq!(
        dispatch_abandoned(&cla, segments).await,
        cla::Acceptance::Refused
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(app_rx.is_empty(), "an abandoned transfer must not deliver");
    assert!(
        forwarded_rx.is_empty(),
        "an abandoned transfer is never reported"
    );
}

// A bundle from the remote source to the local application, requesting
// reception and deletion reports.
fn reporting_bundle(payload_len: usize) -> Bytes {
    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.42".parse().unwrap())
        .with_flags(Flags {
            receipt_report_requested: true,
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Owned(vec![0xA5_u8; payload_len]))
        .build(CreationTimestamp::now())
        .unwrap();
    Bytes::from(data)
}

// A 20 KB-payload reporting bundle cut to 8 KB: the payload's byte-string
// header still claims 20 KB, but the body is short.
fn short_bundle() -> Bytes {
    reporting_bundle(20_000).slice(..8_000)
}

// A node that observes how a short transfer ends: status reports on, the
// local application registered, and a peer for the remote source's node —
// the route for the reports (report-to defaults to the source).
async fn short_final_setup() -> (
    Bpa,
    Arc<TestApp>,
    flume::Receiver<(Eid, Bytes)>,
    Arc<PipelineCla>,
    flume::Receiver<Bytes>,
) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();
    (bpa, app, app_rx, cla, forwarded_rx)
}

// Dispatches `short` in `chunk`-byte segments ending in a `Final` (a single
// `Final` for `None`), and asserts it is rejected with one report.
async fn assert_short_final_is_reported(short: Bytes, chunk: Option<usize>) {
    let mut stream = SegmentReceiver::new(&short, chunk.unwrap_or(short.len()));
    assert_rejected_with_one_report(&mut stream).await;
}

// Dispatches `stream`, a complete transfer of an invalid bundle, and asserts
// the transfer is accepted, the bundle never delivered, and exactly one
// status report leaves: reception and deletion asserted, the deletion citing
// `BlockUnintelligible`.
async fn assert_rejected_with_one_report(stream: &mut SegmentReceiver) {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let (bpa, _app, app_rx, cla, forwarded_rx) = short_final_setup().await;

    // The timeout only bounds a regression: a `Final` handed to the payload
    // drain after the header pass took it would await an exhausted stream.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        cla.sink.get().unwrap().dispatch(None, None, stream),
    )
    .await
    .expect("a complete transfer must not hang the ingress task");
    assert!(
        matches!(result, Ok(cla::Acceptance::Accepted)),
        "an invalid bundle in a complete transfer is accepted and dropped: {result:?}"
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(Duration::from_secs(5), forwarded_rx.recv_async())
        .await
        .expect("Timeout waiting for the status report")
        .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only the status report may leave the node"
    );
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, "ipn:0.2.1".parse::<Eid>().unwrap());
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnintelligible);

    // The completed shutdown is the barrier proving nothing else left the
    // node, and that the bundle was never delivered.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
    assert!(
        app_rx.is_empty(),
        "the invalid bundle must not be delivered"
    );
}

// A payload CRC mismatch is reported however the bundle arrives: whole in one
// `Final`, or whole in a `Next` that an empty `Final` closes. The payload tail
// checks the trailer in the buffer as it would in a later segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_crc_mismatch_is_reported_however_it_arrives() {
    let mut bad = reporting_bundle(1000).to_vec();
    // The last byte of the CRC-32 value: the trailer is `44 c0 c1 c2 c3 FF`.
    let last_crc_byte = bad.len() - 2;
    bad[last_crc_byte] ^= 0xFF;
    let bad = Bytes::from(bad);

    assert_short_final_is_reported(bad.clone(), None).await;

    let mut stream = SegmentReceiver {
        segments: Mutex::new(VecDeque::from([
            cla::Segment::Next(bad),
            cla::Segment::Final(Bytes::new()),
        ])),
    };
    assert_rejected_with_one_report(&mut stream).await;
}

// R-04: the ingress size cap refuses an over-cap bundle at both of
// `parse_headers`' enforcement points — the accumulated header bytes and the
// declared whole-bundle size, each a `HeaderFailure::TooLarge` — surfacing
// `Acceptance::Refused` so the CLA withholds the transfer ack. The declared
// case refuses without awaiting the payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_size_cap_refuses_oversized_bundle() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, &vec![0xA5_u8; 20_000]);

    // Header phase: a cap below the first accumulated segment trips
    // `parse_headers` before the header chain even completes.
    {
        let bpa = Bpa::builder()
            .node_ids(node_ids.clone())
            .max_bundle_size(NonZeroU64::new(256).unwrap())
            .build()
            .await
            .unwrap();
        bpa.start(false).await;
        let (cla, _fwd) = PipelineCla::new();
        bpa.register_cla(
            "test".to_string(),
            cla.clone(),
            None,
            cla::ClaInit::default(),
        )
        .await
        .unwrap();
        let mut stream = SegmentReceiver::new(&inbound, 1000);
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut stream)
                .await
                .unwrap(),
            cla::Acceptance::Refused,
            "header-phase over-cap must be refused"
        );
        bpa.shutdown().await;
    }

    // Declared phase: a cap that admits the header chain (parse yields
    // `Partial`) but not the declared payload refuses in the header pass,
    // before a single payload byte is drained — proved by a source that parks
    // forever after the header chunks. The timeout only bounds a regression.
    {
        let bpa = Bpa::builder()
            .node_ids(node_ids.clone())
            .max_bundle_size(NonZeroU64::new(10_000).unwrap())
            .build()
            .await
            .unwrap();
        bpa.start(false).await;
        let (cla, _fwd) = PipelineCla::new();
        bpa.register_cla(
            "test".to_string(),
            cla.clone(),
            None,
            cla::ClaInit::default(),
        )
        .await
        .unwrap();
        let mut stream = ParkedReceiver::new(&inbound, 1000, 3);
        assert_eq!(
            tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                cla.sink.get().unwrap().dispatch(None, None, &mut stream)
            )
            .await
            .expect("Timeout: the declared over-cap refusal must not wait for the payload")
            .unwrap(),
            cla::Acceptance::Refused,
            "declared over-cap must be refused pre-drain"
        );
        bpa.shutdown().await;
    }
}

// A payload declaration whose wire form ends at `u64::MAX` leaves no room
// for the outer break. The header pass rejects it as invalid, so the CLA door
// disposes of it (accepted, never stored) without pulling a payload byte and
// without computing the bundle's length, which would overflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_disposes_of_a_payload_declared_to_u64_max() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;
    let (cla, _fwd) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    // A built bundle with its payload block replaced by a CRC-32 payload
    // header (`86 01 01 04 02 5b` + an 8-byte length) whose extent ends at
    // exactly u64::MAX:
    //   extent.end = block_start + header(14) + len + crc_trailer(5)
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let good = build_bundle(&remote_source, &local_dest, b"x");
    let block_start = parse(good.clone())
        .expect("built bundle parses")
        .bundle
        .blocks
        .get(&1)
        .expect("payload block")
        .extent
        .start;
    let len = u64::MAX - block_start - 19;
    let mut crafted = good[..usize::try_from(block_start).unwrap()].to_vec();
    crafted.extend_from_slice(&[0x86, 0x01, 0x01, 0x04, 0x02, 0x5b]);
    crafted.extend_from_slice(&len.to_be_bytes());

    // The headers arrive whole and the source then parks forever, so a door
    // that admitted the declaration would wait on the payload. The timeout
    // only bounds a regression.
    let mut stream = ParkedReceiver::new(&crafted, crafted.len(), 1);
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink.get().unwrap().dispatch(None, None, &mut stream)
        )
        .await
        .expect("Timeout: the rejection must not wait for the payload")
        .unwrap(),
        cla::Acceptance::Accepted,
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// PERF-01: Throughput (REQ-13: >1000 bundles/sec)
// ---------------------------------------------------------------------------

/// Measures bundle forwarding throughput through the BPA pipeline.
/// Dispatches bundles via CLA, routes to a peer, signals on the other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throughput() {
    print_system_info();
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, arrival_rx) = TimedCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let src: Eid = "ipn:0.3.1".parse().unwrap();
    let dst: Eid = "ipn:0.2.99".parse().unwrap();
    let count = 1000usize;

    // Pre-generate all bundles with unique IDs (avoids construction overhead in measurement)
    let warmup_bundles: Vec<_> = (0..10)
        .map(|_| build_bundle(&src, &dst, b"warmup"))
        .collect();
    let test_bundles: Vec<_> = (0..count)
        .map(|_| build_bundle(&src, &dst, b"throughput"))
        .collect();

    // Warm up
    for (i, mut bundle) in warmup_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Measure — serial dispatch+receive to avoid backpressure and duplicate issues.
    // Each bundle is dispatched and received before the next is sent.
    let start = tokio::time::Instant::now();
    let mut last_arrival = start;
    for (i, mut bundle) in test_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        last_arrival =
            // Event-driven wait; the timeout only bounds a regression.
            tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
                .await
                .unwrap_or_else(|_| {
                    panic!("Timeout waiting for throughput bundle {i} (of {count})")
                })
                .unwrap();
    }
    let elapsed = last_arrival - start;

    let bundles_per_sec = count as f64 / elapsed.as_secs_f64();
    eprintln!("Throughput: {count} bundles in {elapsed:.2?} = {bundles_per_sec:.0} bundles/sec",);

    // REQ-13 (>1000 bundles/sec, in memory) is measured by the criterion
    // benchmark, not gated: a wall-clock gate here fails under build load.
    // This test pins that every bundle arrives, and reports the rate.

    // Don't call bpa.shutdown() — 1000 ForwardPending bundles in metadata
    // cause the internal poller to re-poll indefinitely during shutdown.
    // The BPA is leaked; the runtime cleans up on test exit.
}

// ---------------------------------------------------------------------------
// PERF-LAT-01: Forwarding Latency
// ---------------------------------------------------------------------------

/// Measures per-bundle forwarding latency through the BPA pipeline.
/// Unidirectional: CLA dispatch → BPA route → CLA forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarding_latency() {
    print_system_info();
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, arrival_rx) = TimedCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let src: Eid = "ipn:0.3.1".parse().unwrap();
    let dst: Eid = "ipn:0.2.99".parse().unwrap();
    let count = 100usize;

    // Pre-generate all bundles with unique IDs
    let warmup_bundles: Vec<_> = (0..10)
        .map(|_| build_bundle(&src, &dst, b"warmup"))
        .collect();
    let test_bundles: Vec<_> = (0..count)
        .map(|_| build_bundle(&src, &dst, b"latency"))
        .collect();

    // Warm up
    for (i, mut bundle) in warmup_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Measure individual dispatch-to-forward latencies.
    // The arrival time is sampled inside forward(), so we measure the
    // actual pipeline processing time, not the channel wait.
    let mut latencies = Vec::with_capacity(count);

    for (i, mut bundle) in test_bundles.into_iter().enumerate() {
        let dispatched = tokio::time::Instant::now();
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        let arrived =
            // Event-driven wait; the timeout only bounds a regression.
            tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
                .await
                .unwrap_or_else(|_| panic!("Timeout waiting for bundle {i} (of {count})"))
                .unwrap();
        latencies.push(arrived - dispatched);
    }

    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let p99 = latencies[latencies.len() * 99 / 100];

    eprintln!("Forwarding latency ({count} bundles): P50={p50:.2?} P95={p95:.2?} P99={p99:.2?}");

    // Drop receiver to unblock any poller send_async, then shutdown
    drop(arrival_rx);
    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-06: Egress filter bundle/data consistency
// ---------------------------------------------------------------------------

// The block type the extent-checking Rewriter inserts.
const INSERTED: Type = Type::Unrecognised(200);

/// Records any divergence between the Bundle's block extents and the wire
/// data it is handed alongside, observed through the reader's extent-based
/// block access: a stale block map over the bytes reads back shifted
/// garbage. Then inserts a block, shifting the payload extent the per-hop
/// rewrite works over next.
struct ExtentCheckRewriter {
    mismatch: Arc<Mutex<Option<String>>>,
}

impl Rewriter for ExtentCheckRewriter {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        let reader = ctx.reader();
        let mut mismatch = self.mismatch.lock().unwrap();

        // The payload extent must index the bytes.
        match reader.block(1) {
            Some((_, Availability::Available(payload))) if payload.as_ref() == b"Hello remote" => {}
            Some((_, Availability::Available(payload))) => {
                *mismatch = Some(format!("payload extent skewed: {:?}", payload.as_ref()))
            }
            Some((_, unavailable)) => {
                *mismatch = Some(format!("payload unavailable: {unavailable:?}"))
            }
            None => *mismatch = Some("no payload block".to_string()),
        }

        if let Err(e) = ctx.editor().insert(
            INSERTED,
            BlockFlags::default(),
            CrcType::None,
            emit(&42u64).0.into(),
        ) {
            *mismatch = Some(format!("the insert was refused: {e}"));
        }
    }
}

/// The Egress Rewriters see the stored (bundle, data) pair consistently, and
/// hand the per-hop rewrite a consistent rewritten pair: the transmitted
/// bundle carries the Rewriter's insert, this node's Previous Node and the
/// payload, each decoding from its own extent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_filter_sees_consistent_extents() {
    let mismatch = Arc::new(Mutex::new(None));
    let mut pack = FilterPack::new("test");
    pack.egress_rewriter(
        "extent-check",
        ExtentCheckRewriter {
            mismatch: mismatch.clone(),
        },
    );
    let bpa = Bpa::builder().add_filters(pack).build().await.unwrap();
    bpa.start(false).await;

    // Register CLA and add a peer for the remote node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, &[remote_node])
        .await
        .unwrap();

    // Register an application and send a bundle to the remote node — a
    // locally-originated bundle has no Previous Node block, so the per-hop
    // rewrite inserts one after the Rewriter's insert
    let (app, _app_rx) = TestApp::new();
    let source = bpa
        .register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.99".parse().unwrap(),
            Bytes::from_static(b"Hello remote"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    assert_eq!(
        *mismatch.lock().unwrap(),
        None,
        "Egress filter saw an inconsistent (bundle, data) pair"
    );

    let out = parse(forwarded).expect("the transmitted bundle parses");
    let block = |block_type: Type| {
        out.bundle
            .blocks
            .values()
            .find(|b| b.block_type == block_type)
            .expect("the block is transmitted")
    };
    assert_eq!(
        block(INSERTED).payload(&out.data),
        Some(emit(&42u64).0.as_slice()),
        "the Rewriter's insert"
    );
    let Eid::Ipn { fqnn, .. } = source else {
        panic!("the node has an ipn identity");
    };
    assert_eq!(
        block(Type::PreviousNode)
            .extract::<Eid>(&out.data)
            .expect("the Previous Node decodes")
            .expect("the Previous Node is resident"),
        Eid::Ipn {
            fqnn,
            service_number: 0
        },
        "this node's admin endpoint"
    );
    assert_eq!(
        out.bundle.blocks[&1].payload(&out.data),
        Some(b"Hello remote".as_slice())
    );

    bpa.shutdown().await;
}

/// Keys traffic for the unrouted ipn:0.3 node onto ipn:0.2.1 — the
/// SR-shaped per-bundle override, honouring the skip-self discipline by
/// keying only a foreign destination.
struct KeyingClassifier;

impl Classifier for KeyingClassifier {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        if let Eid::Ipn { fqnn, .. } = &ctx.bundle().primary.destination
            && fqnn.node_number == 3
        {
            delta.route_key = Some("ipn:0.2.1".parse().unwrap());
        }
        Verdict::Continue(delta)
    }
}

/// A Classifier-written route_key persists in the record's metadata and
/// drives the RIB lookup: a bundle whose destination has no route forwards
/// via the route for its key. Without the key threading the bundle parks
/// Waiting forever, so the forward event is the proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classifier_route_key_drives_the_lookup() {
    let mut pack = FilterPack::new("test");
    pack.originate_classifier("keyer", KeyingClassifier);
    let bpa = Bpa::builder().add_filters(pack).build().await.unwrap();
    bpa.start(false).await;

    // A peer exists for ipn:0.2 only; ipn:0.3 has no route.
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[remote_node],
        )
        .await
        .unwrap();

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            "ipn:0.3.99".parse().unwrap(),
            Bytes::from_static(b"Keyed onward"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the keyed bundle to forward")
    .expect("Channel closed");

    bpa.shutdown().await;
}

/// Keys traffic for the unregistered local service ipn:0.1.77 onto
/// ipn:0.2.1: a foreign key for a local destination.
struct LocalKeyingClassifier;

impl Classifier for LocalKeyingClassifier {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        if ctx.bundle().primary.destination == "ipn:0.1.77".parse().unwrap() {
            delta.route_key = Some("ipn:0.2.1".parse().unwrap());
        }
        Verdict::Continue(delta)
    }
}

// A bundle parks under the inputs its lookup walked: one addressed to an
// unregistered local service but keyed onto a foreign EID waits for a route
// to its key, not for its destination's service, so the key's route
// appearing forwards it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keyed_local_destination_waits_for_its_key_route() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier("keyer", LocalKeyingClassifier);
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .add_filters(pack)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    // A fresh arrival executes its routing decision before the CLA's
    // dispatch returns, so the bundle has parked once it does: nothing
    // routes its key yet.
    let (bundle, data) = Builder::new("ipn:0.3.1".parse().unwrap(), "ipn:0.1.77".parse().unwrap())
        .with_payload(Cow::Borrowed(b"Keyed away".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut Bytes::from(data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The key's route appears.
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout: the keyed bundle must wait for its key's route")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id, bundle.primary.id);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Failing Application — always returns Err from on_deliver
// ---------------------------------------------------------------------------

struct FailingApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    /// Fires once on_deliver completes (after constructing the Err), so a
    /// waiter observes the point at which the dispatcher's Err-handling
    /// branch is about to run rather than the point at which it started.
    completed_tx: flume::Sender<()>,
}

impl FailingApp {
    fn new() -> (Arc<Self>, flume::Receiver<()>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                completed_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl services::Application for FailingApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let err = services::Error::Internal("test: simulated delivery failure".into());
        let _ = self.completed_tx.send(());
        Err(err)
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// INT-BPA-05: dispatcher tolerates on_deliver returning Err
// ---------------------------------------------------------------------------

/// When `on_deliver` returns `Err`, the dispatcher must preserve the bundle
/// as `WaitingForService`, not report it delivered and delete it. Proven
/// end-to-end: after the failing receiver rejects the bundle, a fresh working
/// receiver registered on the same service id must have it re-delivered. A
/// regression to the old drop-on-error behaviour makes the re-delivery time
/// out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_handles_on_deliver_err() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (sender, _sender_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(43), sender.clone())
        .await
        .unwrap();

    let (failing, completed_rx) = FailingApp::new();
    let receiver_eid = bpa
        .register_application(Service::Ipn(42), failing.clone())
        .await
        .unwrap();

    sender
        .sink
        .get()
        .unwrap()
        .send(
            receiver_eid,
            Bytes::from_static(b"payload"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The failing receiver rejects the bundle from within on_deliver.
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        completed_rx.recv_async(),
    )
    .await
    .expect("dispatcher must call on_deliver")
    .unwrap();

    // Swap in a working receiver on the same service id — no quiesce
    // needed: the failed delivery's park re-checks the routing snapshot, so
    // whether it lands before this registration's WaitingForService poll or
    // after it, the bundle re-enters dispatch and reaches the new receiver.
    failing.sink.get().unwrap().unregister().await;
    let (receiver, received_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), receiver.clone())
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let (_source, payload) = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        received_rx.recv_async(),
    )
    .await
    .expect("parked bundle must be re-delivered on re-registration")
    .unwrap();
    assert_eq!(payload, Bytes::from_static(b"payload"));

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Deferring CLA — answers Accepted for the first N offers, Sent afterwards,
// and reports every offered bundle id to the test
// ---------------------------------------------------------------------------

struct DeferringCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    offers_tx: flume::Sender<Id>,
    remaining_accepts: AtomicUsize,
}

impl DeferringCla {
    fn new(accepts: usize) -> (Arc<Self>, flume::Receiver<Id>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                offers_tx: tx,
                remaining_accepts: AtomicUsize::new(accepts),
            }),
            rx,
        )
    }

    fn sink(&self) -> &dyn cla::Sink {
        self.sink.get().unwrap().as_ref()
    }
}

#[async_trait]
impl cla::Cla for DeferringCla {
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
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.offers_tx.send(bundle_id.clone());
        if self
            .remaining_accepts
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            Ok(cla::ForwardBundleResult::Accepted)
        } else {
            Ok(cla::ForwardBundleResult::Sent)
        }
    }
}

// Helper: BPA with a DeferringCla registered and a peer for ipn:0.<node>
async fn deferring_setup(
    accepts: usize,
    peer_node_number: u32,
) -> (hardy_bpa::bpa::Bpa, Arc<DeferringCla>, flume::Receiver<Id>) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, offers_rx) = DeferringCla::new(accepts);
    bpa.register_cla(
        format!("deferring-{peer_node_number}"),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    cla.sink()
        .add_peer(
            cla::ClaAddress::Private(format!("peer-{peer_node_number}").into_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: peer_node_number,
            })],
        )
        .await
        .unwrap();

    (bpa, cla, offers_rx)
}

async fn expect_offer(rx: &flume::Receiver<Id>) -> Id {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv_async())
        .await
        .expect("Timeout waiting for CLA offer")
        .expect("Channel closed")
}

// Known test-guide deviation (quiet-window absence helper):
// scheduled for the dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
async fn expect_no_offer(rx: &flume::Receiver<Id>) {
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv_async())
            .await
            .is_err(),
        "Unexpected CLA offer"
    );
}

// ---------------------------------------------------------------------------
// INT-BPA-07: deferred outcome — Failed re-enters dispatch per-bundle
// ---------------------------------------------------------------------------

/// A transfer answered `Accepted` whose outcome is reported `Failed` gets a
/// fresh routing decision and is re-offered to the CLA; the bundle is never
/// dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_failed_redispatches() {
    let (bpa, cla, offers_rx) = deferring_setup(1, 2).await;

    assert_eq!(
        cla.sink()
            .dispatch(
                None,
                None,
                &mut build_bundle(
                    &"ipn:0.3.1".parse().unwrap(),
                    &"ipn:0.2.99".parse().unwrap(),
                    b"deferred-fail",
                ),
            )
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_rx).await;
    cla.sink()
        .transfer_outcome(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();

    // The failed transfer re-enters dispatch and is re-offered (the route is
    // unchanged); the second offer is answered Sent by the mock.
    let id2 = expect_offer(&offers_rx).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-08: deferred outcome — Completed resolves the transfer
// ---------------------------------------------------------------------------

/// A transfer answered `Accepted` whose outcome is reported `Completed` is
/// complete: no re-offer, a late duplicate outcome is ignored, and a
/// re-arrival of the same bundle is dropped as a duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_completed_resolves() {
    let (bpa, cla, offers_rx) = deferring_setup(1, 2).await;

    let mut data = build_bundle(
        &"ipn:0.3.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"deferred-ok",
    );
    assert_eq!(
        cla.sink()
            .dispatch(None, None, &mut data.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_rx).await;
    cla.sink()
        .transfer_outcome(&id, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    // A late duplicate outcome is ignored, not honoured twice.
    cla.sink()
        .transfer_outcome(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    expect_no_offer(&offers_rx).await;

    // The completed bundle's id was committed: a re-arrival of the same
    // bundle is dropped as a duplicate rather than re-forwarded.
    assert_eq!(
        cla.sink().dispatch(None, None, &mut data).await.unwrap(),
        cla::Acceptance::Accepted
    );
    expect_no_offer(&offers_rx).await;

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-09: deferred outcome — peer removal resolves outcome-unknown
// ---------------------------------------------------------------------------

/// Removing the peer while a transfer awaits its outcome resolves it as
/// outcome-unknown: the bundle returns to Waiting and is re-offered when the
/// peer comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_peer_removal_resolves_unknown() {
    let (bpa, cla, offers_rx) = deferring_setup(1, 2).await;

    assert_eq!(
        cla.sink()
            .dispatch(
                None,
                None,
                &mut build_bundle(
                    &"ipn:0.3.1".parse().unwrap(),
                    &"ipn:0.2.99".parse().unwrap(),
                    b"outcome-unknown",
                ),
            )
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_rx).await;

    let peer_addr = cla::ClaAddress::Private("peer-2".as_bytes().into());
    let peer_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    assert!(cla.sink().remove_peer(&peer_addr).await.unwrap());
    assert!(
        cla.sink()
            .add_peer(peer_addr, core::slice::from_ref(&peer_node))
            .await
            .unwrap()
    );

    // The unresolved transfer went back to Waiting on peer removal, and the
    // re-added peer's route re-dispatches it.
    let id2 = expect_offer(&offers_rx).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-10: deferred outcome — only the owning CLA may resolve a transfer
// ---------------------------------------------------------------------------

/// An outcome reported by a CLA that does not own the transfer's peer is
/// ignored; the owning CLA's subsequent outcome is still honoured. Outcomes
/// for unknown bundles are ignored without error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_ignores_wrong_cla() {
    let (bpa, cla_a, offers_a) = deferring_setup(1, 2).await;

    // A second CLA with its own peer on a different node
    let (cla_b, offers_b) = DeferringCla::new(0);
    bpa.register_cla(
        "deferring-b".to_string(),
        cla_b.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    cla_b
        .sink()
        .add_peer(
            cla::ClaAddress::Private("peer-b".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 4,
            })],
        )
        .await
        .unwrap();

    assert_eq!(
        cla_a
            .sink()
            .dispatch(
                None,
                None,
                &mut build_bundle(
                    &"ipn:0.3.1".parse().unwrap(),
                    &"ipn:0.2.99".parse().unwrap(),
                    b"wrong-cla",
                ),
            )
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_a).await;

    // An outcome for a bundle the BPA has never seen is ignored without error.
    let unknown = Id {
        source: "ipn:0.9.9".parse().unwrap(),
        timestamp: CreationTimestamp::now(),
        fragment_info: None,
    };
    cla_b
        .sink()
        .transfer_outcome(&unknown, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    // CLA B does not own the transfer's peer: its outcome is ignored.
    cla_b
        .sink()
        .transfer_outcome(&id, cla::TransferOutcome::Completed)
        .await
        .unwrap();
    expect_no_offer(&offers_a).await;

    // The owning CLA's outcome is still honoured.
    cla_a
        .sink()
        .transfer_outcome(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    let id2 = expect_offer(&offers_a).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");
    expect_no_offer(&offers_b).await;

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-11: expiry during a CLA-owned transfer is covered by
// tests/forward_expiry.rs — the reaper defers a ForwardAckPending bundle,
// and the outcome resolves it (a failed transfer expires at the dispatch
// expiry checkpoint).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Blocking CLA — holds the first forward open until released, then fails it
// synchronously; every subsequent offer is answered Sent. All offers are
// recorded.
// ---------------------------------------------------------------------------

struct BlockingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    offered_tx: flume::Sender<Id>,
    release_rx: flume::Receiver<()>,
    first: AtomicBool,
}

impl BlockingCla {
    fn new() -> (Arc<Self>, flume::Receiver<Id>, flume::Sender<()>) {
        let (offered_tx, offered_rx) = flume::bounded(16);
        let (release_tx, release_rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                offered_tx,
                release_rx,
                first: AtomicBool::new(true),
            }),
            offered_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl cla::Cla for BlockingCla {
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
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.offered_tx.send_async(bundle_id.clone()).await;
        if self.first.swap(false, Ordering::SeqCst) {
            let _ = self.release_rx.recv_async().await;
            return Err(cla::Error::StreamCancelled);
        }
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// A route event that lands while a transfer is in flight is not missed:
/// the event's poll cannot see the bundle (claimed ForwardAckPending), but
/// the synchronous failure's park re-checks the routing snapshot captured
/// when the flight began and re-enters dispatch — the only path to the
/// second offer, since no further route events occur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_failure_park_recheck_redispatches() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, offered_rx, release_tx) = BlockingCla::new();
    bpa.register_cla(
        "blocking".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-a".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(90), app.clone())
        .await
        .unwrap();
    let id = app
        .sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.7".parse().unwrap(),
            Bytes::from_static(b"recheck"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The transfer is in flight, held open by the CLA...
    // (every timeout below only bounds a regression)
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("Timeout waiting for the first offer")
            .unwrap();
    assert_eq!(offered, id);

    // ...when a second peer for the same node appears. Its route event
    // polls Waiting bundles and cannot see this one.
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-b".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    // Release: the synchronous failure parks the bundle; the park detects
    // the mid-flight routing change and re-dispatches.
    release_tx.send(()).expect("CLA gone");
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("The park's recheck must re-dispatch the bundle")
            .unwrap();
    assert_eq!(offered, id);

    bpa.shutdown().await;
}

/// A synchronous forward failure must not resurrect a bundle that was
/// resolved while its transfer was in flight: the failure's Waiting park is
/// conditional and loses against the terminal claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_failure_never_resurrects_resolved_bundle() {
    let metadata_store = Arc::new(MetadataMemStorage::new(None));
    let bpa = Bpa::builder()
        .metadata_storage(metadata_store.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, offered_rx, release_tx) = BlockingCla::new();
    bpa.register_cla(
        "blocking".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-a".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(91), app.clone())
        .await
        .unwrap();
    let id = app
        .sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.7".parse().unwrap(),
            Bytes::from_static(b"resolve me"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The transfer is in flight (the timeout only bounds a regression)...
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("Timeout waiting for the offer")
            .unwrap();
    assert_eq!(offered, id);

    // ...when the bundle is resolved, exactly as the reaper's terminal
    // claim would leave it (delete wins every race).
    metadata_store.tombstone(&id).await.unwrap();

    // Release: the failure's park must lose against the tombstone.
    // shutdown() is the barrier — it joins the egress pollers, so the park
    // has fully run by the time it returns; no quiet window is involved.
    release_tx.send(()).expect("CLA gone");
    bpa.shutdown().await;

    assert!(
        metadata_store.get(&id).await.unwrap().is_none(),
        "a resolved bundle was resurrected by a failed transfer's park"
    );
    let (live_tx, live_rx) = hardy_async::channel::bounded(4);
    metadata_store.poll_waiting(&live_tx).await.unwrap();
    drop(live_tx);
    assert!(
        live_rx.recv().await.is_err(),
        "a resolved bundle re-entered Waiting"
    );
}

// ---------------------------------------------------------------------------
// The config-gated RFC 9171 validity checks at the pre-drain gate
// ---------------------------------------------------------------------------

/// Feeds `data` to the BPA through the CLA sink as one Final segment.
async fn dispatch_inbound(cla: &PipelineCla, data: Bytes) {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let producer = tokio::spawn(async move {
        hardy_async::channel::Sender::send(&tx, Segment::Final(data))
            .await
            .unwrap();
    });
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut rx)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    producer.await.unwrap();
}

/// Builds a strict-or-relaxed BPA around a local application at ipn:0.1.42,
/// returning the delivery channel.
async fn gate_fixture(
    configure: impl FnOnce(hardy_bpa::builder::BpaBuilder) -> hardy_bpa::builder::BpaBuilder,
) -> (Bpa, Arc<PipelineCla>, flume::Receiver<(Eid, Bytes)>) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = configure(Bpa::builder().node_ids(node_ids))
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    (bpa, cla, app_rx)
}

fn unprotected_primary_bundle() -> Bytes {
    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.42".parse().unwrap())
        .with_crc_type(hardy_bpv7::crc::CrcType::None)
        .with_payload(Cow::Borrowed(b"no integrity".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

fn clockless_ageless_bundle() -> Bytes {
    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.42".parse().unwrap())
        .with_payload(Cow::Borrowed(b"no clock".as_slice()))
        .build(CreationTimestamp::default())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

/// RFC 9171 §4.3.1: with the default strict config, a primary block with
/// neither CRC nor BIB coverage is rejected at the pre-drain gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_rejects_unprotected_primary_block() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b).await;

    dispatch_inbound(&cla, unprotected_primary_bundle()).await;

    // Shutdown is the barrier: an admitted bundle would have completed
    // delivery before it returns.
    bpa.shutdown().await;
    assert!(
        app_rx.is_empty(),
        "an unprotected primary block must be rejected at the gate"
    );
}

/// `primary_block_integrity(false)` relaxes the §4.3.1 check: the same
/// bundle is admitted and delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaxed_gate_admits_unprotected_primary_block() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b.primary_block_integrity(false)).await;

    dispatch_inbound(&cla, unprotected_primary_bundle()).await;

    // Event-driven wait; the timeout only bounds a regression.
    let (_, payload) =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for delivery")
            .expect("Channel closed");
    assert_eq!(payload.as_ref(), b"no integrity");

    bpa.shutdown().await;
}

/// RFC 9171 §4.4.2: a clockless bundle without a Bundle Age block is
/// malformed, and its parse refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clockless_bundle_without_age_is_refused() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b).await;

    dispatch_inbound(&cla, clockless_ageless_bundle()).await;

    // Shutdown is the barrier: an admitted bundle would have completed
    // delivery before it returns.
    bpa.shutdown().await;
    assert!(
        app_rx.is_empty(),
        "a clockless bundle without a Bundle Age block must be refused"
    );
}

// A peer removed while its construction is still in flight leaves nothing
// behind (whole-codebase review #14): add_peer claims the address, builds
// the peer complete, publishes, then re-checks its claim before installing
// RIB entries — a concurrent remove wins the claim, and the half-added peer
// is withdrawn (add_peer reports false). The policy's controller
// construction is the interception point: it parks until released, and a
// fresh add on the same address afterwards succeeds cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_removed_mid_construction_is_withdrawn() {
    use hardy_bpa::policy;

    struct ParkedPolicy {
        entered_tx: flume::Sender<()>,
        release_rx: flume::Receiver<()>,
    }

    struct DefaultController {
        queue: Arc<dyn policy::EgressQueue>,
    }

    #[async_trait]
    impl policy::FlowController for DefaultController {
        fn queue_for(&self) -> u32 {
            0
        }

        async fn forward(&self, _queue: u32, bundle: hardy_bpa::bundle::Bundle) {
            self.queue.forward(bundle).await
        }
    }

    #[async_trait]
    impl policy::FlowControllerFactory for ParkedPolicy {
        fn queue_count(&self) -> NonZeroU32 {
            NonZeroU32::MIN
        }

        async fn new_controller(
            &self,
            queues: policy::EgressQueueSet,
        ) -> Arc<dyn policy::FlowController> {
            let _ = self.entered_tx.send(());
            // Parked until the test drops the release sender; the peer is
            // mid-construction for exactly this window.
            let _ = self.release_rx.recv_async().await;
            Arc::new(DefaultController {
                queue: queues.next_free,
            })
        }
    }

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (entered_tx, entered_rx) = flume::unbounded();
    let (release_tx, release_rx) = flume::bounded::<()>(1);
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        Some(Arc::new(ParkedPolicy {
            entered_tx,
            release_rx,
        })),
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });

    // The add parks in controller construction...
    let add = {
        let cla = cla.clone();
        let peer_addr = peer_addr.clone();
        let remote_node = remote_node.clone();
        tokio::spawn(async move {
            cla.sink
                .get()
                .unwrap()
                .add_peer(peer_addr, from_ref(&remote_node))
                .await
        })
    };
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), entered_rx.recv_async())
        .await
        .expect("Timeout waiting for controller construction to start")
        .expect("Policy gone");

    // ...and the removal wins the claim while it is parked.
    assert!(
        cla.sink
            .get()
            .unwrap()
            .remove_peer(&peer_addr)
            .await
            .expect("remove_peer failed"),
        "the claimed address is removable mid-construction"
    );

    drop(release_tx);
    let added = add
        .await
        .expect("add task panicked")
        .expect("add_peer failed");
    assert!(!added, "a removed claim must not complete as an added peer");

    // The address is free and a fresh add succeeds (the release sender is
    // dropped, so its construction completes immediately).
    assert!(
        cla.sink
            .get()
            .unwrap()
            .add_peer(peer_addr, from_ref(&remote_node))
            .await
            .expect("add_peer failed"),
        "a fresh add on the freed address succeeds"
    );

    bpa.shutdown().await;
}

// Routing is decided at the ingress gate: a destination resolving to an
// explicit Drop route rejects the bundle before its payload is drained —
// the stream is left unconsumed and nothing spools. Drop-with-reason
// reports like any gated drop (the combined reception + deletion record);
// Drop-without-reason is silent, with no reception report either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_route_drop_never_awaits_the_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A peer for the report route (report-to defaults to the source), plus
    // Drop routes: reasoned for one destination, silent for another.
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();
    bpa.register_routing_agent(
        "drop-routes".to_string(),
        Arc::new(StaticRoutingAgent::new(&[
            (
                "ipn:0.9.99".parse().unwrap(),
                RouteAction::Drop(Some(ReasonCode::NoKnownRouteToDestinationFromHere)),
                1,
            ),
            ("ipn:0.9.98".parse().unwrap(), RouteAction::Drop(None), 1),
        ])),
    )
    .await
    .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let oversized = |dest: &str| {
        let (_, data) = Builder::new(remote_source.clone(), dest.parse().unwrap())
            .with_flags(Flags {
                receipt_report_requested: true,
                delete_report_requested: true,
                ..Default::default()
            })
            .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");
        data
    };

    // Reasoned Drop: accepted with one combined report, while the payload
    // source is parked forever — the verdict returning at all proves the
    // decision never awaited the drain, and a rejected arrival spools
    // nothing. The timeout only bounds a regression.
    let data = oversized("ipn:0.9.99");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink
                .get()
                .unwrap()
                .dispatch(Some(&remote_node), None, &mut stream)
        )
        .await
        .expect("Timeout: the route drop must not wait for the payload")
        .unwrap(),
        cla::Acceptance::Accepted,
        "a route-dropped bundle is accepted and terminated, never refused"
    );
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(parsed.bundle.primary.flags.is_admin_record);
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::NoKnownRouteToDestinationFromHere);

    // Silent Drop: accepted while the source is parked, no report at all.
    let data = oversized("ipn:0.9.98");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink
                .get()
                .unwrap()
                .dispatch(Some(&remote_node), None, &mut stream)
        )
        .await
        .expect("Timeout: the silent route drop must not wait for the payload")
        .unwrap(),
        cla::Acceptance::Accepted
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "a reasonless Drop route emits nothing"
    );
}

/// A routing agent that hands its sink to the test, which installs and
/// withdraws routes at chosen moments.
struct SinkAgent {
    sink: hardy_async::sync::spin::Once<Box<dyn RoutingSink>>,
}

#[async_trait]
impl RoutingAgent for SinkAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}
}

// The gate's decision of record is made again at the commit when the table
// changed during the drain, so a route withdrawn while the payload was still
// arriving does not carry the bundle. The route moves from one peer to
// another while the source is parked mid-payload, and the bundle leaves
// through the new route's peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_changed_during_the_drain_is_decided_again() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    // Two CLAs with one peer each: ipn:0.2 behind the first, ipn:0.4 behind
    // the second.
    let (old_cla, old_rx) = PipelineCla::new();
    bpa.register_cla(
        "old".to_string(),
        old_cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    old_cla
        .sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("old".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();
    let (new_cla, new_rx) = PipelineCla::new();
    bpa.register_cla(
        "new".to_string(),
        new_cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    new_cla
        .sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("new".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 4,
            })],
        )
        .await
        .unwrap();

    let agent = Arc::new(SinkAgent {
        sink: hardy_async::sync::spin::Once::new(),
    });
    bpa.register_routing_agent("swap".to_string(), agent.clone())
        .await
        .unwrap();
    let routes = agent.sink.get().unwrap();
    let old_via = RouteAction::Via("ipn:0.2.0".parse().unwrap());
    let new_via = RouteAction::Via("ipn:0.4.0".parse().unwrap());
    assert!(
        routes
            .add_route("ipn:0.3.*".parse().unwrap(), old_via.clone(), 1)
            .await
            .unwrap()
    );

    let (bundle, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.1".parse().unwrap())
        .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let data = Bytes::from(data);
    let start = usize::try_from(bundle.blocks[&1].payload_range().start).unwrap();

    // Each send on the one-slot channel completes only once the previous
    // segment has been pulled. The header pass takes the head alone (no
    // filter declares a peek), so the drain pulling the first payload
    // segment proves the gate's lookup is done.
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let arrival = old_cla.clone();
    let dispatch = tokio::spawn(async move {
        arrival
            .sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut rx)
            .await
    });
    tx.send(Segment::Next(data.slice(..start))).await.unwrap();
    tx.send(Segment::Next(data.slice(start..start + 1000)))
        .await
        .unwrap();
    tx.send(Segment::Next(data.slice(start + 1000..start + 2000)))
        .await
        .unwrap();

    // The gate decided on the old route; move it to the new peer while the
    // payload is still arriving.
    assert!(
        routes
            .add_route("ipn:0.3.*".parse().unwrap(), new_via, 1)
            .await
            .unwrap()
    );
    assert!(
        routes
            .remove_route(&"ipn:0.3.*".parse().unwrap(), &old_via, 1)
            .await
            .unwrap()
    );
    tx.send(Segment::Final(data.slice(start + 2000..)))
        .await
        .unwrap();
    drop(tx);

    // The timeout only bounds a regression.
    assert_eq!(
        tokio::time::timeout(tokio::time::Duration::from_secs(5), dispatch)
            .await
            .expect("Timeout waiting for the arrival to settle")
            .unwrap()
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(tokio::time::Duration::from_secs(5), new_rx.recv_async())
        .await
        .expect("Timeout: the bundle must leave by the route in force at the commit")
        .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id, bundle.primary.id);

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(old_rx.is_empty(), "the withdrawn route carries nothing");
}

/// An application whose status notifications block until the test releases
/// them, announcing each as it begins.
struct BlockingNotifyApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    entered_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

#[async_trait]
impl services::Application for BlockingNotifyApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        let _ = self.entered_tx.send(());
        let _ = self.release_rx.recv_async().await;
    }
}

// An arrival's admin-endpoint arm runs on the processing pool, not on the
// arriving CLA's task: the embedder callbacks it makes do not hold the
// session's acknowledgement. The CLA's dispatch returns while the status
// notification is still blocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admin_record_does_not_hold_the_arriving_cla() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (entered_tx, entered_rx) = flume::bounded(1);
    let (release_tx, release_rx) = flume::bounded(1);
    bpa.register_application(
        Service::Ipn(42),
        Arc::new(BlockingNotifyApp {
            sink: hardy_async::sync::spin::Once::new(),
            entered_tx,
            release_rx,
        }),
    )
    .await
    .unwrap();
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    // A delivery report, from a remote node's admin endpoint to ours, about
    // a bundle the application sent.
    let (record, ()) = emit(&AdministrativeRecord::BundleStatusReport(
        BundleStatusReport {
            bundle_id: Id {
                source: "ipn:0.1.42".parse().unwrap(),
                timestamp: CreationTimestamp::now(),
                fragment_info: None,
            },
            delivered: Some(StatusAssertion(None)),
            ..Default::default()
        },
    ));
    let (_, data) = Builder::new("ipn:0.2.0".parse().unwrap(), "ipn:0.1.0".parse().unwrap())
        .with_flags(Flags {
            is_admin_record: true,
            ..Default::default()
        })
        .with_payload(Cow::Owned(record))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");

    // The timeout only bounds a regression.
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut Bytes::from(data))
        )
        .await
        .expect("Timeout: the admin record must not hold the arriving CLA")
        .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), entered_rx.recv_async())
        .await
        .expect("Timeout waiting for the status notification")
        .expect("Channel closed");
    release_tx.send(()).unwrap();

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// The originate door: the validation split, route-at-door, and the caller's
// error surface
// ---------------------------------------------------------------------------

/// Routing is decided at the originate door: a destination resolving to an
/// explicit Drop route rejects the send before its payload is drained — the
/// stream is left unconsumed and nothing spools. Unlike the CLA gate there
/// is never a status report, report flags notwithstanding: the error return
/// is the originator's notification, and even a reasonless (silent) Drop
/// route surfaces as `Dropped(None)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originate_route_drop_never_awaits_the_payload() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;
    bpa.register_routing_agent(
        "drop-routes".to_string(),
        Arc::new(StaticRoutingAgent::new(&[
            (
                "ipn:0.9.99".parse().unwrap(),
                RouteAction::Drop(Some(ReasonCode::NoKnownRouteToDestinationFromHere)),
                1,
            ),
            ("ipn:0.9.98".parse().unwrap(), RouteAction::Drop(None), 1),
        ])),
    )
    .await
    .unwrap();

    let oversized = |dest: &str| {
        let (_, data) = Builder::new(source_eid.clone(), dest.parse().unwrap())
            .with_flags(Flags {
                receipt_report_requested: true,
                delete_report_requested: true,
                ..Default::default()
            })
            .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");
        data
    };

    // Reasoned Drop: the error returns while the payload source is parked
    // forever — returning at all proves the decision never awaited the
    // drain, and the cancelled spool persists nothing. The timeout only
    // bounds a regression.
    let data = oversized("ipn:0.9.99");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.get().unwrap().send(&mut stream),
    )
    .await
    .expect("Timeout: the route drop must not wait for the payload");
    assert!(matches!(
        result,
        Err(services::Error::Dropped(Some(
            ReasonCode::NoKnownRouteToDestinationFromHere
        )))
    ));

    // Silent Drop: the originator still gets an error where the CLA gate
    // drops silently.
    let data = oversized("ipn:0.9.98");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.get().unwrap().send(&mut stream),
    )
    .await
    .expect("Timeout: the silent route drop must not wait for the payload");
    assert!(matches!(result, Err(services::Error::Dropped(None))));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "an originate rejection must not emit status reports"
    );
}

// The ADU doors refuse a bundle that expires at creation, as the raw door's
// admission gate does: a zero lifetime surfaces as `Dropped(LifetimeExpired)`
// with nothing admitted, whole-buffer or streamed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adu_originate_refuses_a_zero_lifetime() {
    let (bpa, _svc, forwarded_rx, _) = streamed_originate_setup().await;
    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let result = app
        .sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.1".parse().unwrap(),
            Bytes::from_static(b"expired"),
            Duration::ZERO,
            None,
        )
        .await;
    assert!(matches!(
        result,
        Err(services::Error::Dropped(Some(ReasonCode::LifetimeExpired)))
    ));

    // The streamed door, the same refusal before the payload is pulled: the
    // source pends forever if pulled. The timeout only bounds a regression.
    let mut stream = ParkedReceiver::new(b"expired", 7, 0);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        app.sink.get().unwrap().send_streamed(
            "ipn:0.2.1".parse().unwrap(),
            7,
            &mut stream,
            Duration::ZERO,
            None,
        ),
    )
    .await
    .expect("Timeout: the expired send must not pull the payload");
    assert!(matches!(
        result,
        Err(services::Error::Dropped(Some(ReasonCode::LifetimeExpired)))
    ));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing was admitted");
}

/// The raw door carries the validation the ADU door doesn't: hop-exhausted
/// service bytes trip the lifetime/hop admission check, and the
/// config-gated RFC 9171 validity checks (strict by default) now apply to
/// raw sends at the same seat — each surfacing as `Dropped(reason)` to the
/// caller, with nothing admitted. A clockless bundle without a Bundle Age
/// block is malformed, and the parse refuses it as `InvalidBundle`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_applies_the_admission_gates() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;
    let dest: Eid = "ipn:0.2.1".parse().unwrap();

    // Hop-exhausted (count past limit): the lifetime/hop admission pair.
    let (_, data) = Builder::new(source_eid.clone(), dest.clone())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(1).unwrap(),
            count: 2,
        })
        .with_payload(Cow::Borrowed(b"exhausted".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let result = svc.sink.get().unwrap().send(&mut Bytes::from(data)).await;
    assert!(matches!(
        result,
        Err(services::Error::Dropped(Some(ReasonCode::HopLimitExceeded)))
    ));

    // Unprotected primary block: RFC 9171 §4.3.1, newly applied to raw
    // sends.
    let (_, data) = Builder::new(source_eid.clone(), dest.clone())
        .with_crc_type(CrcType::None)
        .with_payload(Cow::Borrowed(b"no integrity".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let result = svc.sink.get().unwrap().send(&mut Bytes::from(data)).await;
    assert!(matches!(
        result,
        Err(services::Error::Dropped(Some(
            ReasonCode::BlockUnintelligible
        )))
    ));

    // Clockless without a Bundle Age block: malformed (RFC 9171 §4.4.2),
    // so its parse refuses it.
    let (_, data) = Builder::new(source_eid.clone(), dest)
        .with_payload(Cow::Borrowed(b"no clock".as_slice()))
        .build(CreationTimestamp::default())
        .expect("Failed to build bundle");
    let result = svc.sink.get().unwrap().send(&mut Bytes::from(data)).await;
    assert!(matches!(
        result,
        Err(services::Error::InvalidBundle(Bpv7Error::MissingBundleAge))
    ));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing was admitted");
}

/// A service-built fragment is rejected at the raw door: fragmentation is
/// a forwarding-time action (RFC 9171 §5.8), so a source never emits
/// fragments — the send surfaces `FragmentedBundle` and nothing is
/// admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_rejects_a_fragment() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;
    let dest: Eid = "ipn:0.2.1".parse().unwrap();

    // A valid bundle, rewritten as a self-declared fragment: the editor
    // sets the fragment flag from the fragment_info and re-emits canonical
    // bytes, so only the fragment fact can trip the door.
    let (_, data) = Builder::new(source_eid, dest)
        .with_payload(Cow::Borrowed(b"a piece of an adu".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let Parsed {
        bundle: raw, data, ..
    } = parse(Bytes::from(data)).expect("parse the built bundle");
    let editor = Editor::new(&raw, &data)
        .with_fragment_info(Some(FragmentInfo {
            offset: 0,
            total_adu_length: 1024,
        }))
        .map_err(|(_, e)| e)
        .expect("set fragment info");
    let chunks = editor.rebuild().expect("rebuild the fragment");
    let mut fragment = Chunk::flatten_bytes(chunks, data);

    let result = svc.sink.get().unwrap().send(&mut fragment).await;
    assert!(matches!(result, Err(services::Error::FragmentedBundle)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the fragment must not be admitted");
}

/// A duplicate raw send surfaces `DuplicateBundle` to the caller — service
/// bundles carry service-authored ids, so the door cannot retry for them —
/// and the first copy is unaffected: forwarded exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_duplicate_surfaces_to_the_caller() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"send once");

    let mut first = data.clone();
    let id = svc
        .sink
        .get()
        .unwrap()
        .send(&mut first)
        .await
        .expect("first send admits");

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");
    assert_eq!(
        parse(forwarded).expect("parse forwarded").bundle.primary.id,
        id
    );

    let mut second = data;
    let result = svc.sink.get().unwrap().send(&mut second).await;
    assert!(matches!(result, Err(services::Error::DuplicateBundle)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the duplicate must not forward");
}

// The originate twin of the gate-side duplicate probe: once the first send
// commits, a replay through the raw door is refused with `DuplicateBundle`
// while its payload source is still parked — the caller's stream is never
// drained for a bundle the node already holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_duplicate_never_awaits_the_payload() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    // Oversized payload so the replay genuinely streams (headers < payload).
    let payload = vec![0x5A_u8; 20_000];
    let data = build_bundle(&source_eid, &dest, &payload);

    let mut first = data.clone();
    svc.sink
        .get()
        .unwrap()
        .send(&mut first)
        .await
        .expect("first send admits");
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    // Replay with the payload parked: the duplicate must surface from the
    // gate probe, not from an insert that waited on the whole payload. The
    // timeout only bounds a regression: a door that waits for the payload
    // parks forever.
    let mut replay = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.get().unwrap().send(&mut replay),
    )
    .await
    .expect("Timeout: the duplicate origination awaited its parked payload");
    assert!(
        matches!(result, Err(services::Error::DuplicateBundle)),
        "the parked replay must surface DuplicateBundle without the payload"
    );

    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the duplicate must not forward");
}

// A node whose storage survives a restart, with a peer for ipn:0.2.
async fn restart_setup(
    metadata: Arc<MetadataMemStorage>,
    bundles: Arc<BundleMemStorage>,
) -> (
    Bpa,
    Arc<EchoService>,
    Arc<PipelineCla>,
    flume::Receiver<Bytes>,
) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .metadata_storage(metadata)
        .bundle_storage(bundles)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    let svc = EchoService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::new();
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
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();
    (bpa, svc, cla, forwarded_rx)
}

// The number of bundles `bundles` holds.
async fn stored_bundles(bundles: &BundleMemStorage) -> usize {
    let (tx, rx) = hardy_async::channel::unbounded();
    bundles.recover(&tx).await.unwrap();
    drop(tx);
    let mut count = 0;
    while rx.recv().await.is_ok() {
        count += 1;
    }
    count
}

// The duplicate refusal of record is the metadata insert; the recent-id
// cache answers only for what this process committed. Bundles forwarded
// before a restart arrive again after it, the cache now cold: a CLA arrival
// is accepted and refused at the insert, a raw send surfaces
// `DuplicateBundle`, neither is forwarded again, and neither copy's spooled
// data is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_after_a_restart_is_refused_at_the_insert() {
    let metadata = Arc::new(MetadataMemStorage::new(None));
    let bundles = Arc::new(BundleMemStorage::new(None, None));
    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let arrival = build_bundle(&"ipn:0.3.1".parse().unwrap(), &dest, b"arrives once");
    let raw = build_bundle(&"ipn:0.1.7".parse().unwrap(), &dest, b"sent once");

    // First life: both forward. The timeouts only bound a regression.
    let (bpa, svc, cla, forwarded_rx) = restart_setup(metadata.clone(), bundles.clone()).await;
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut arrival.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    svc.sink
        .get()
        .unwrap()
        .send(&mut raw.clone())
        .await
        .expect("the first raw send admits");
    for _ in 0..2 {
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for a first-life forward")
        .expect("Channel closed");
    }
    bpa.shutdown().await;

    // Second life over the same storage.
    let held = stored_bundles(&bundles).await;
    let (bpa, svc, cla, forwarded_rx) = restart_setup(metadata, bundles.clone()).await;
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut arrival.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    let result = svc.sink.get().unwrap().send(&mut raw.clone()).await;
    assert!(matches!(result, Err(services::Error::DuplicateBundle)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "neither duplicate forwards");
    assert_eq!(
        stored_bundles(&bundles).await,
        held,
        "neither duplicate's spooled data is kept"
    );
}

/// An origination declaring more than `max_bundle_size` is refused before a
/// byte spools, at both doors: the raw door from its headers' declared size
/// (the header pass), the ADU door from its declared payload. Both payload
/// sources park after their first segments, so a refusal returning at all
/// proves the door never drained the payload, and each refusal reports a
/// declared size rather than a count of bytes taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_originate_refuses_before_the_payload() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .max_bundle_size(NonZeroU64::new(2048).unwrap())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let svc = EchoService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();
    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    // Raw: the headers declare the whole ~50 KB bundle. The timeout only
    // bounds a regression.
    let (_, data) = Builder::new("ipn:0.1.7".parse().unwrap(), "ipn:0.2.1".parse().unwrap())
        .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.get().unwrap().send(&mut stream),
    )
    .await
    .expect("Timeout: the raw size refusal must not wait for the payload");
    match result {
        Err(services::Error::PayloadTooLarge { size, max }) => {
            assert_eq!(size, data.len() as u64, "the declared wire size");
            assert_eq!(max, 2048);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    // ADU: a 50 KB declared payload, streamed. The timeout only bounds a
    // regression.
    let payload = vec![0u8; 50_000];
    let mut stream = ParkedReceiver::new(&payload, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        app.sink.get().unwrap().send_streamed(
            "ipn:0.2.1".parse().unwrap(),
            50_000,
            &mut stream,
            Duration::from_secs(60),
            None,
        ),
    )
    .await
    .expect("Timeout: the ADU size refusal must not wait for the payload");
    match result {
        Err(services::Error::PayloadTooLarge { size, max }) => {
            assert!(size >= 50_000, "a declared size, got {size}");
            assert_eq!(max, 2048);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    bpa.shutdown().await;
}

// An ADU whose payload fits `max_bundle_size` but whose headers carry the
// bundle over it is refused by the shared stage's declared-size bound before
// the payload is pulled: the source parks on its first pull. The declared
// size is the built bundle's, whose length depends on the width of the
// creation timestamp's sequence number, so it is checked against twins built
// at one creation time with a sequence number of each CBOR width.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adu_originate_refuses_headers_over_the_cap() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .max_bundle_size(NonZeroU64::new(2048).unwrap())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (app, _app_rx) = TestApp::new();
    let source = bpa
        .register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let destination: Eid = "ipn:0.2.1".parse().unwrap();
    let lifetime = Duration::from_secs(60);

    let payload = vec![0u8; 2040];
    let creation_time = DtnTime::now();
    let twins: Vec<u64> = [0, 24, 256, 65_536]
        .into_iter()
        .map(|sequence_number| {
            let (_, data) = Builder::new(source.clone(), destination.clone())
                .with_lifetime(lifetime)
                .with_payload(Cow::Borrowed(&payload))
                .build(CreationTimestamp::from_parts(
                    Some(creation_time),
                    sequence_number,
                ))
                .expect("Failed to build bundle");
            data.len() as u64
        })
        .collect();
    assert!(
        twins.iter().all(|&size| size > 2048),
        "the headers carry every twin over the cap: {twins:?}"
    );

    // The timeout only bounds a regression.
    let mut stream = ParkedReceiver::new(&payload, 1000, 0);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        app.sink
            .get()
            .unwrap()
            .send_streamed(destination, 2040, &mut stream, lifetime, None),
    )
    .await
    .expect("Timeout: the size refusal must not wait for the payload");
    match result {
        Err(services::Error::PayloadTooLarge { size, max }) => {
            assert!(
                twins.contains(&size),
                "expected one of {twins:?}, got {size}"
            );
            assert_eq!(max, 2048);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    bpa.shutdown().await;
}

/// A builder-configured CLA goes live at `start`, not at `build`: activation
/// waits for storage recovery, so the quiescent-store contract covers the
/// configuration path exactly as it covers dynamic registrations.
#[tokio::test]
async fn configured_endpoints_activate_at_start() {
    struct ProbeCla {
        registered: hardy_async::sync::spin::Once<()>,
    }

    #[async_trait]
    impl cla::Cla for ProbeCla {
        async fn on_register(
            &self,
            _sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
            self.registered.call_once(|| ());
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            _bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            Ok(cla::ForwardBundleResult::Sent)
        }
    }

    let probe = Arc::new(ProbeCla {
        registered: hardy_async::sync::spin::Once::new(),
    });
    let bpa = Bpa::builder()
        .cla("probe", probe.clone(), None, cla::ClaInit::default())
        .build()
        .await
        .unwrap();
    assert!(
        probe.registered.get().is_none(),
        "a configured CLA must not go live before start"
    );

    bpa.start(false).await;
    assert!(
        probe.registered.get().is_some(),
        "start activates the configured CLA"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// The streamed ADU door: ApplicationSink::send_streamed
// ---------------------------------------------------------------------------

/// A payload streamed through `send_streamed` originates a bundle
/// wire-equivalent to the whole-buffer `send` of the same payload: same
/// source, destination, flags, and payload bytes (the ids differ only by
/// creation timestamp), and the streamed form parses canonically — the CRC
/// computed as the payload flowed is the CRC a resident build writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_matches_whole_buffer_send() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let payload = vec![0xB7_u8; 9000];

    // Streamed: several Next segments and a Final.
    let (tx, mut rx) = hardy_async::channel::bounded(8);
    for chunk in payload.chunks(2000) {
        tx.send(Segment::Next(Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    tx.send(Segment::Final(Bytes::new())).await.unwrap();
    drop(tx);
    let streamed_id = app
        .sink
        .get()
        .unwrap()
        .send_streamed(
            dest.clone(),
            payload.len() as u64,
            &mut rx,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("streamed send failed");

    // Whole-buffer twin.
    let buffered_id = app
        .sink
        .get()
        .unwrap()
        .send(
            dest.clone(),
            Bytes::from(payload.clone()),
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("whole-buffer send failed");

    // Event-driven waits; the timeouts only bound a regression.
    let mut parsed = Vec::new();
    for _ in 0..2 {
        let forwarded = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for forwarded bundle")
        .expect("Channel closed");
        parsed.push(parse(forwarded).expect("forwarded bundle parses"));
    }
    parsed.sort_by_key(|p| p.bundle.primary.id != streamed_id);
    let (streamed, buffered) = (&parsed[0], &parsed[1]);

    assert_eq!(streamed.bundle.primary.id, streamed_id);
    assert_eq!(buffered.bundle.primary.id, buffered_id);
    assert_eq!(streamed.bundle.primary.destination, dest);
    assert_eq!(streamed.bundle.primary.flags, buffered.bundle.primary.flags);
    assert_eq!(
        streamed.bundle.blocks.len(),
        buffered.bundle.blocks.len(),
        "same block structure"
    );
    // Past the primary, which carries each bundle's own id, the two are the
    // same bytes.
    let after_primary = |p: &Parsed| {
        let end = usize::try_from(p.bundle.blocks[&0].extent.end).unwrap();
        p.data.slice(end..)
    };
    assert_eq!(after_primary(streamed), after_primary(buffered));
    assert_eq!(
        streamed.bundle.blocks[&1]
            .payload(&streamed.data)
            .expect("streamed payload resident"),
        payload.as_slice()
    );

    bpa.shutdown().await;
}

/// A producer that goes away before its final segment cancels the streamed
/// ADU send: `StreamCancelled`, nothing originated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_producer_drop_cancels() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let (tx, mut rx) = hardy_async::channel::bounded(2);
    tx.send(Segment::Next(Bytes::from(vec![0u8; 1000])))
        .await
        .unwrap();
    drop(tx); // no Final — the producer aborts

    let result = app
        .sink
        .get()
        .unwrap()
        .send_streamed(
            "ipn:0.2.1".parse().unwrap(),
            5000,
            &mut rx,
            Duration::from_secs(3600),
            None,
        )
        .await;
    assert!(matches!(result, Err(services::Error::StreamCancelled)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "a cancelled streamed send must not originate a bundle"
    );
}

/// The declared-length contract is enforced with nothing persisted: an
/// over-delivering producer is rejected `PayloadTooLarge`, an
/// under-delivering one `PayloadUnderrun`, each carrying the declared
/// total.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_enforces_the_declared_length() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();
    let dest: Eid = "ipn:0.2.1".parse().unwrap();

    // Overrun: 2000 bytes against a declaration of 1000.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    tx.send(Segment::Final(Bytes::from(vec![0u8; 2000])))
        .await
        .unwrap();
    drop(tx);
    let result = app
        .sink
        .get()
        .unwrap()
        .send_streamed(dest.clone(), 1000, &mut rx, Duration::from_secs(3600), None)
        .await;
    assert!(matches!(
        result,
        Err(services::Error::PayloadTooLarge { max: 1000, .. })
    ));

    // Underrun: 1000 bytes against a declaration of 2000.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    tx.send(Segment::Final(Bytes::from(vec![0u8; 1000])))
        .await
        .unwrap();
    drop(tx);
    let result = app
        .sink
        .get()
        .unwrap()
        .send_streamed(dest.clone(), 2000, &mut rx, Duration::from_secs(3600), None)
        .await;
    assert!(matches!(
        result,
        Err(services::Error::PayloadUnderrun {
            size: 1000,
            expected: 2000
        })
    ));

    // A declaration past `max_bundle_size` is refused with its declared
    // length before the payload is pulled, however large: `u64::MAX` would
    // otherwise overflow the built bundle's extents. The timeout only bounds
    // a regression.
    let mut stream = ParkedReceiver::new(&[], 1, 0);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        app.sink.get().unwrap().send_streamed(
            dest,
            u64::MAX,
            &mut stream,
            Duration::from_secs(3600),
            None,
        ),
    )
    .await
    .expect("Timeout: the size refusal must not wait for the payload");
    assert!(matches!(
        result,
        Err(services::Error::PayloadTooLarge { size: u64::MAX, .. })
    ));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing was originated");
}

/// The trait's provided `send_streamed` buffers and delegates to `send`
/// with the same error surface, so third-party sinks (the gRPC bridge
/// included) get the streamed API without implementing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_default_buffers_and_delegates() {
    struct RecordingSink(flume::Sender<Bytes>);

    #[async_trait]
    impl services::ApplicationSink for RecordingSink {
        async fn unregister(&self) {}

        async fn send(
            &self,
            _destination: Eid,
            data: Bytes,
            _lifetime: Duration,
            _options: Option<services::SendOptions>,
        ) -> services::Result<Id> {
            self.0.send(data).unwrap();
            Ok(Id::default())
        }
    }

    let (tx, sent_rx) = flume::bounded(1);
    let sink = RecordingSink(tx);

    // A clean stream is buffered whole and handed to `send`.
    let payload = vec![0x11_u8; 3000];
    let (seg_tx, mut rx) = hardy_async::channel::bounded(4);
    for chunk in payload.chunks(1024) {
        seg_tx
            .send(Segment::Next(Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    seg_tx.send(Segment::Final(Bytes::new())).await.unwrap();
    drop(seg_tx);
    services::ApplicationSink::send_streamed(
        &sink,
        "ipn:0.2.1".parse().unwrap(),
        payload.len() as u64,
        &mut rx,
        Duration::from_secs(60),
        None,
    )
    .await
    .expect("default send_streamed delegates");
    assert_eq!(sent_rx.recv().unwrap().as_ref(), payload.as_slice());

    // The declared-length contract holds before `send` is ever called.
    let (seg_tx, mut rx) = hardy_async::channel::bounded(2);
    seg_tx
        .send(Segment::Final(Bytes::from(vec![0u8; 100])))
        .await
        .unwrap();
    drop(seg_tx);
    let result = services::ApplicationSink::send_streamed(
        &sink,
        "ipn:0.2.1".parse().unwrap(),
        500,
        &mut rx,
        Duration::from_secs(60),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(services::Error::PayloadUnderrun {
            size: 100,
            expected: 500
        })
    ));
    assert!(sent_rx.is_empty(), "a violating stream never reaches send");
}

// A bundle from ipn:0.2.1, node 1's peer, so a report to it routes, to
// `destination`, whose 6 KiB payload, carrying a CRC-32, its source
// signed, where `signed`, then encrypted where `encrypted`, encrypting the
// BIB over it too (RFC 9172 §3.9), and asking for deletion reports: the
// bundle, the payload's plaintext and the keys.
#[cfg(feature = "rfc9173")]
fn with_protected_payload(
    destination: &str,
    signed: bool,
    encrypted: bool,
) -> (Bytes, Vec<u8>, Vec<Key>) {
    protected_payload(destination, signed, encrypted, CrcType::CRC32_CASTAGNOLI)
}

// `with_protected_payload`, the payload carrying no CRC, as RFC 9173
// §3.8.1 and §4.8.1 leave a target, so that a tamper reaches the security
// check rather than the CRC.
#[cfg(feature = "rfc9173")]
fn with_protected_crcless_payload(
    destination: &str,
    signed: bool,
    encrypted: bool,
) -> (Bytes, Vec<u8>, Vec<Key>) {
    protected_payload(destination, signed, encrypted, CrcType::None)
}

#[cfg(feature = "rfc9173")]
fn protected_payload(
    destination: &str,
    signed: bool,
    encrypted: bool,
    crc_type: CrcType,
) -> (Bytes, Vec<u8>, Vec<Key>) {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let plaintext: Vec<u8> = (0..6144u32).map(|i| (i % 239) as u8).collect();
    let (_, data) = Builder::new(source.clone(), destination.parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .add_extension_block(Type::Payload)
        .unwrap()
        .with_flags(BlockFlags {
            delete_bundle_on_failure: true,
            ..Default::default()
        })
        .with_crc_type(crc_type)
        .build(plaintext.as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let (sign_key, enc_key) = generated_keys();
    let mut data = Bytes::from(data);
    if signed {
        let parsed = parse(data).unwrap();
        data = Bytes::from(
            Signer::new(&parsed.bundle, &parsed.data)
                .sign_block(
                    1,
                    signer::Context::HMAC_SHA2(ScopeFlags::default()),
                    source.clone(),
                    &sign_key,
                )
                .map_err(|(_, e)| e)
                .unwrap()
                .rebuild()
                .unwrap(),
        );
    }
    if encrypted {
        let parsed = parse(data).unwrap();
        data = Bytes::from(
            Encryptor::new(&parsed.bundle, &parsed.data)
                .encrypt_block(
                    1,
                    EncryptContext::AES_GCM(ScopeFlags::default()),
                    source,
                    &enc_key,
                )
                .map_err(|(_, e)| e)
                .unwrap()
                .rebuild()
                .unwrap(),
        );
    }
    (data, plaintext, vec![sign_key, enc_key])
}

// Whether every bundle node 1 stored from `source` it has since deleted:
// nothing of the source's is left in storage.
#[cfg(feature = "rfc9173")]
fn nothing_left_from(
    saved_rx: &flume::Receiver<Bytes>,
    deleted_rx: &flume::Receiver<Bytes>,
    source: &Eid,
) -> bool {
    let from = |rx: &flume::Receiver<Bytes>| {
        rx.drain()
            .filter(|data| &parse(data.clone()).unwrap().bundle.primary.id.source == source)
            .count()
    };
    from(saved_rx) == from(deleted_rx)
}

// The first bundle from `source` node 1 forwards. A status report it sends
// first must not assert a deletion: a bundle the drain's verdict rejected is
// deleted after its save, so the save alone does not show it was kept.
#[cfg(feature = "rfc9173")]
async fn forwarded_from(sent_rx: &flume::Receiver<Bytes>, source: &Eid) -> Parsed {
    loop {
        // Event-driven wait; the timeout only bounds a regression.
        let sent = tokio::time::timeout(Duration::from_secs(5), sent_rx.recv_async())
            .await
            .expect("timeout waiting for the forwarded bundle")
            .expect("channel closed");
        let sent = parse(sent).expect("a sent bundle parses");
        if sent.bundle.primary.flags.is_admin_record {
            let body = sent.bundle.blocks[&1]
                .payload(&sent.data)
                .expect("the report's payload is resident");
            let AdministrativeRecord::BundleStatusReport(status) =
                hardy_cbor::decode::parse(body).expect("the payload is an admin record");
            assert!(
                status.deleted.is_none(),
                "the bundle was deleted, {:?}, not forwarded",
                status.reason
            );
            continue;
        }
        if &sent.bundle.primary.id.source == source {
            return sent;
        }
    }
}

// The deletion report node 1 sent for a bundle it received: its reason.
#[cfg(feature = "rfc9173")]
async fn deletion_reason(sent_rx: &flume::Receiver<Bytes>) -> ReasonCode {
    let status = sent_status_report(sent_rx).await;
    assert!(status.deleted.is_some(), "deletion asserted");
    status.reason
}

// The destination accepts the payload's operations at ingress (RFC 9172
// §1.4, §5.1.1): an encrypted payload, its BIB under the same encryption, is
// decrypted as it streams, the BIB verified against the plaintext, and the
// bundle stored with the payload in plaintext, its CRC type as received and
// the CRC over the plaintext, and no security block. Delivery then has nothing to
// decrypt. A payload received in plaintext under a BIB goes the same way:
// the BIB is verified as the payload streams and removed. Whether the
// payload arrives whole or in segments, which the header pass holds only a
// prefix of, makes no difference.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_stores_an_accepted_payload_in_plaintext() {
    for segments in [1, 3] {
        for (signed, encrypted) in [(false, true), (true, true), (true, false)] {
            let case = format!("{segments} segments, signed {signed}, encrypted {encrypted}");
            let (inbound, plaintext, keys) = with_protected_payload(LOCAL, signed, encrypted);
            let (bpa, saved_rx, _, _) =
                keyed_node_receiving_in(keys, None, inbound, segments).await;
            let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
            assert_eq!(
                stored.bundle.blocks[&1].payload(&stored.data),
                Some(plaintext.as_slice()),
                "{case}"
            );
            assert!(stored.bcbs.is_empty() && stored.bibs.is_empty(), "{case}");
            assert_eq!(
                stored.bundle.blocks[&1].crc_type,
                CrcType::CRC32_CASTAGNOLI,
                "{case}"
            );
            bpa.shutdown().await;
        }
    }
}

// A relay whose Classifier announces the payload accepts its operations, a
// node that is not the destination processing a BCB "if directed to do so
// as a matter of security policy" (RFC 9172 §5.1.1): whether the payload
// arrives whole or in segments, it is stored in plaintext, without its BCB
// and BIB, its CRC type as received.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_accepts_an_announced_payload() {
    for segments in [1, 3] {
        for (signed, encrypted) in [(false, true), (true, true), (true, false)] {
            let case = format!("{segments} segments, signed {signed}, encrypted {encrypted}");
            let (inbound, plaintext, keys) = with_protected_payload(REMOTE, signed, encrypted);
            let (bpa, saved_rx, _, _) =
                keyed_node_receiving_in(keys, accepting(&[Type::Payload]), inbound, segments).await;
            let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
            assert_eq!(
                stored.bundle.blocks[&1].payload(&stored.data),
                Some(plaintext.as_slice()),
                "{case}"
            );
            assert!(stored.bcbs.is_empty() && stored.bibs.is_empty(), "{case}");
            assert_eq!(
                stored.bundle.blocks[&1].crc_type,
                CrcType::CRC32_CASTAGNOLI,
                "{case}"
            );
            bpa.shutdown().await;
        }
    }
}

// A fragment's payload is part of the bundle's, whose operations are
// accepted after reassembly: a Classifier's announcement of it is passed
// over, and the fragment is stored as received.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_leaves_an_announced_fragment_payload_as_received() {
    let (inbound, _, keys) = with_protected_payload(REMOTE, true, true);
    let fragment = as_fragment(inbound);

    let (bpa, saved_rx, _) =
        keyed_node_receiving(keys, accepting(&[Type::Payload]), fragment.clone()).await;
    let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
    assert_eq!(stored.data, fragment, "stored byte for byte as received");
    bpa.shutdown().await;
}

// An acceptor trumps a verification: a Hop Count this node is the acceptor
// of, kept as received because it holds no key for the BIB over it, is not
// discarded for a designation that it be verified.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_hop_count_this_node_cannot_check_is_still_kept() {
    let (inbound, hop_count, _, _, enc_key) = with_hop_count_signed_with(Type::Unrecognised(199));

    let (bpa, saved_rx, _) =
        keyed_node_receiving(vec![enc_key], verifying(&[Type::HopCount]), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        stored.bundle.blocks[&hop_count].bcb.is_some(),
        "the Hop Count stays as received"
    );
    bpa.shutdown().await;
}

// Likewise a block announced for acceptance whose acceptance fails and
// leaves it as received, tied by its encrypted BIB to the payload: its
// designation for verification does not then discard it as a block
// encrypted here.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_designated_block_stays_as_its_failed_acceptance_leaves_it() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, true);
    let received = parse(inbound.clone()).unwrap();
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier(
        "accepts",
        AnnouncesPresent {
            role: Role::Acceptor,
            block_types: vec![Type::Unrecognised(199)],
        },
    );
    pack.ingress_classifier(
        "verifies",
        AnnouncesPresent {
            role: Role::Verifier,
            block_types: vec![Type::Unrecognised(199)],
        },
    );

    let (bpa, saved_rx, _) = keyed_node_receiving(keys, Some(pack), inbound).await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        received.bundle.blocks[&block].payload(&received.data),
        "the block stays as received"
    );
    assert!(
        stored.bundle.blocks[&1].bcb.is_some(),
        "the payload travels encrypted"
    );
    bpa.shutdown().await;
}

// `data` with one byte of block `block`'s block-type-specific data flipped,
// so that its ciphertext no longer authenticates.
#[cfg(feature = "rfc9173")]
fn with_byte_flipped(data: &Bytes, block: u64) -> Bytes {
    let parsed = parse(data.clone()).unwrap();
    let start = usize::try_from(parsed.bundle.blocks[&block].payload_range().start).unwrap();
    let mut data = data.to_vec();
    data[start] ^= 0x01;
    Bytes::from(data)
}

// A block accepted for confidentiality alone is decrypted and stored in
// plaintext without its BCB, and the BIB over it, decrypted with it, stays
// in plaintext for the next node: checked here where the signing key is
// held, and left unchecked, no failure, where it is not.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_accepted_for_confidentiality_keeps_its_bib_in_plaintext() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, false);
    for held in [keys.clone(), vec![keys[1].clone()]] {
        let case = format!("{} keys held", held.len());
        let (bpa, saved_rx, _) = keyed_node_receiving(
            held,
            with_roles(&[(
                Type::Unrecognised(199),
                Roles::each(Role::Acceptor, SecurityServices::CONFIDENTIALITY),
            )]),
            inbound.clone(),
        )
        .await;

        let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
        let accepted = &stored.bundle.blocks[&block];
        assert_eq!(
            accepted.payload(&stored.data),
            Some(b"announced".as_slice()),
            "the block is stored in plaintext, {case}"
        );
        assert!(accepted.bcb.is_none(), "without its BCB, {case}");
        let BibCoverage::Some(bib) = accepted.bib else {
            panic!("the BIB over it stays, {case}");
        };
        assert!(
            stored.bundle.blocks[&bib].bcb.is_none(),
            "in plaintext, {case}"
        );
        assert!(
            stored
                .bundle
                .blocks
                .values()
                .all(|b| b.block_type != Type::BlockSecurity),
            "and no BCB remains, {case}"
        );
        bpa.shutdown().await;
    }
}

// A block encrypted here cannot be accepted for integrity alone: its BIB is
// encrypted with it, and only the acceptor of the BCB operation reads it. A
// failed acceptance, "Block unsupported".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_accepted_for_integrity_while_encrypted_is_a_failed_acceptance() {
    let (inbound, block, keys) = with_protected_block(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        true,
        false,
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        keys,
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Acceptor, SecurityServices::INTEGRITY),
        )]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);
    bpa.shutdown().await;
}

// A block accepted for integrity alone, received in plaintext under a BIB,
// is verified and stored without the BIB.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_accepted_for_integrity_is_stored_without_its_bib() {
    let (inbound, block, key) =
        with_signed_block(REMOTE, BlockFlags::default(), false, Flags::default());

    let (bpa, saved_rx, _) = keyed_node_receiving(
        vec![key],
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Acceptor, SecurityServices::INTEGRITY),
        )]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert_eq!(
        stored.bundle.blocks[&block].payload(&stored.data),
        Some(b"announced".as_slice()),
        "the block stays"
    );
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .all(|b| b.block_type != Type::BlockIntegrity),
        "without its BIB"
    );
    bpa.shutdown().await;
}

// A service announced that the block does not carry is missing: a block
// announced for both services, carrying a BIB only, is a failed acceptance,
// discarded, "Missing security operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_service_announced_that_the_block_does_not_carry_is_missing() {
    let (inbound, block, key) = with_signed_block(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        false,
        Flags::default(),
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        vec![key],
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Acceptor, SecurityServices::BOTH),
        )]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::MissingSecurityOperation);
    bpa.shutdown().await;
}

// A block verified for confidentiality has its ciphertext authenticated
// under the key this node holds, its plaintext going nowhere, and the
// bundle travels as received, BCB and BIB with it.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_verified_for_confidentiality_travels_as_received() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, false);
    let received = parse(inbound.clone()).unwrap();

    let (bpa, saved_rx, _) = keyed_node_receiving(
        keys,
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        stored.bundle.blocks[&block].bcb.is_some(),
        "the block stays encrypted"
    );
    assert_eq!(
        stored.data, received.data,
        "stored byte for byte as received"
    );
    bpa.shutdown().await;
}

// A block verified for confidentiality without its key is a failed
// verification, a block this node cannot process: discarded, "Block
// unsupported". The block is encrypted and unsigned, so no encrypted BIB
// keeps it in the bundle.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_verified_for_confidentiality_without_its_key_is_discarded() {
    let (inbound, block, _) = with_protected_block(
        REMOTE,
        BlockFlags {
            report_on_failure: true,
            ..BlockFlags::default()
        },
        false,
        false,
    );

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        Vec::new(),
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        inbound,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    let status = sent_status_report(&sent_rx).await;
    assert!(status.received.is_some(), "reception asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);
    bpa.shutdown().await;
}

// Ciphertext that does not authenticate is a failed operation: the block
// is discarded with every security block associated with it (RFC 9172
// §5.1.1), the encrypted BIB over it whole.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_block_whose_ciphertext_does_not_authenticate_is_discarded() {
    let (inbound, block, keys) = with_protected_block(REMOTE, BlockFlags::default(), true, false);
    let corrupted = with_byte_flipped(&inbound, block);

    let (bpa, saved_rx, _) = keyed_node_receiving(
        keys,
        with_roles(&[(
            Type::Unrecognised(199),
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        corrupted,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "the block is discarded"
    );
    assert!(
        stored
            .bundle
            .blocks
            .values()
            .all(|b| !matches!(b.block_type, Type::BlockIntegrity | Type::BlockSecurity)),
        "with its security blocks"
    );
    bpa.shutdown().await;
}

// A payload verified for confidentiality is authenticated as it streams,
// its plaintext going nowhere, and the bundle is stored as received.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_payload_verified_for_confidentiality_is_stored_as_received() {
    let (inbound, _, keys) = with_protected_payload(REMOTE, true, true);
    let source: Eid = "ipn:0.2.1".parse().unwrap();

    let (bpa, saved_rx, _, sent_rx) = keyed_node_receiving_in(
        vec![keys[1].clone()],
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        inbound.clone(),
        3,
    )
    .await;

    let stored = stored_from(&saved_rx, &source).await;
    assert_eq!(stored.data, inbound, "stored byte for byte as received");
    let forwarded = forwarded_from(&sent_rx, &source).await;
    assert_eq!(forwarded.bundle.primary.id, stored.bundle.primary.id);
    bpa.shutdown().await;
}

// As above, for a payload that arrives whole in the header pass: the
// resident bytes run past the payload's data, through its CRC and the
// breaks, and the ciphertext authenticated is the payload's alone.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_payload_verified_for_confidentiality_that_arrives_whole_is_stored_as_received() {
    let (inbound, _, keys) = with_protected_payload(REMOTE, true, true);
    let source: Eid = "ipn:0.2.1".parse().unwrap();

    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        vec![keys[1].clone()],
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        inbound.clone(),
    )
    .await;

    let stored = stored_from(&saved_rx, &source).await;
    assert_eq!(stored.data, inbound, "stored byte for byte as received");
    let forwarded = forwarded_from(&sent_rx, &source).await;
    assert_eq!(forwarded.bundle.primary.id, stored.bundle.primary.id);
    bpa.shutdown().await;
}

// A payload verified for confidentiality without its key deletes the
// bundle, which cannot lose its payload, "Block unsupported".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_payload_verified_for_confidentiality_without_its_key_deletes_the_bundle() {
    let (inbound, _, _) = with_protected_payload(REMOTE, true, true);

    let (bpa, saved_rx, _, sent_rx) = keyed_node_receiving_in(
        Vec::new(),
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        inbound,
        3,
    )
    .await;

    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::BlockUnsupported
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.2.1".parse().unwrap());
}

// A payload whose ciphertext does not authenticate as it streams deletes
// the bundle, "Failed security operation".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_payload_whose_ciphertext_does_not_authenticate_deletes_the_bundle() {
    let (inbound, _, keys) = with_protected_crcless_payload(REMOTE, true, true);
    let corrupted = with_byte_flipped(&inbound, 1);

    let (bpa, saved_rx, deleted_rx, sent_rx) = keyed_node_receiving_in(
        vec![keys[1].clone()],
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Verifier, SecurityServices::CONFIDENTIALITY),
        )]),
        corrupted,
        3,
    )
    .await;

    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::FailedSecurityOperation
    );
    // The drain staged the bundle before the ciphertext settled, and the
    // verdict discarded it; the completed shutdown is the barrier.
    bpa.shutdown().await;
    assert!(
        nothing_left_from(&saved_rx, &deleted_rx, &"ipn:0.2.1".parse().unwrap()),
        "the staged bundle is discarded"
    );
}

// A payload accepted for confidentiality alone is decrypted as it streams
// and stored in plaintext without its BCB, the BIB over it staying in
// plaintext, checked here where the key is held.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_payload_accepted_for_confidentiality_keeps_its_bib() {
    let (inbound, plaintext, keys) = with_protected_payload(REMOTE, true, true);

    let (bpa, saved_rx, _, _) = keyed_node_receiving_in(
        keys,
        with_roles(&[(
            Type::Payload,
            Roles::each(Role::Acceptor, SecurityServices::CONFIDENTIALITY),
        )]),
        inbound,
        3,
    )
    .await;

    let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
    let payload = &stored.bundle.blocks[&1];
    assert_eq!(
        payload.payload(&stored.data),
        Some(plaintext.as_slice()),
        "the payload is stored in plaintext"
    );
    assert!(payload.bcb.is_none(), "without its BCB");
    let BibCoverage::Some(bib) = payload.bib else {
        panic!("the BIB over it stays");
    };
    assert!(stored.bundle.blocks[&bib].bcb.is_none(), "in plaintext");
    bpa.shutdown().await;
}

// A node that accepts a block's confidentiality and verifies its integrity
// decrypts the block and checks the BIB over it, now plaintext, which
// stays; without the signing key the verification it was designated fails,
// "Block unsupported".
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_acceptor_of_confidentiality_may_verify_integrity_too() {
    let roles = Roles {
        confidentiality: Some(Role::Acceptor),
        integrity: Some(Role::Verifier),
    };
    let flags = BlockFlags {
        report_on_failure: true,
        ..BlockFlags::default()
    };

    let (inbound, block, keys) = with_protected_block(REMOTE, flags.clone(), true, false);
    let (bpa, saved_rx, _) = keyed_node_receiving(
        keys,
        with_roles(&[(Type::Unrecognised(199), roles)]),
        inbound,
    )
    .await;
    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    let accepted = &stored.bundle.blocks[&block];
    assert!(
        accepted.bcb.is_none() && matches!(accepted.bib, BibCoverage::Some(_)),
        "decrypted, verified, the BIB staying"
    );
    bpa.shutdown().await;

    let (inbound, block, keys) = with_protected_block(REMOTE, flags, true, false);
    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(
        vec![keys[1].clone()],
        with_roles(&[(Type::Unrecognised(199), roles)]),
        inbound,
    )
    .await;
    let stored = stored_from(&saved_rx, &"ipn:0.9.1".parse().unwrap()).await;
    assert!(
        !stored.bundle.blocks.contains_key(&block),
        "unverifiable without the signing key, the block is discarded"
    );
    let status = sent_status_report(&sent_rx).await;
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);
    bpa.shutdown().await;
}

// A fragment's payload is verified after reassembly too: a Classifier's
// designation of it is passed over, and a fragment whose payload this node
// could not verify is stored as received rather than deleted.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_leaves_a_designated_fragment_payload_as_received() {
    let (inbound, _, _) = with_protected_payload(REMOTE, true, true);
    let fragment = as_fragment(inbound);

    let (bpa, saved_rx, _) =
        keyed_node_receiving(Vec::new(), verifying(&[Type::Payload]), fragment.clone()).await;
    let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
    assert_eq!(stored.data, fragment, "stored byte for byte as received");
    bpa.shutdown().await;
}

// The destination without the payload's key, for its BCB or for the BIB
// over it, cannot accept it: the payload, which the bundle cannot lose,
// deletes the bundle as Block unsupported before anything is stored (RFC
// 9171 §5.6 Step 4).
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_without_the_payloads_key_deletes_the_bundle() {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    for segments in [1, 3] {
        // The signing key alone, then the encryption key alone.
        for (signed, encrypted, held) in [(false, true, 0), (true, false, 1)] {
            let case = format!("{segments} segments, signed {signed}, encrypted {encrypted}");
            let (inbound, _, keys) = with_protected_payload(LOCAL, signed, encrypted);
            let (bpa, saved_rx, _, sent_rx) =
                keyed_node_receiving_in(vec![keys[held].clone()], None, inbound, segments).await;
            assert_eq!(
                deletion_reason(&sent_rx).await,
                ReasonCode::BlockUnsupported,
                "{case}"
            );
            bpa.shutdown().await;
            assert!(
                saved_rx
                    .drain()
                    .all(|data| parse(data).unwrap().bundle.primary.id.source != source),
                "{case}: nothing is stored for the deleted bundle"
            );
        }
    }
}

// A payload whose ciphertext does not authenticate, or whose BIB fails over
// the plaintext, deletes the bundle as Failed security operation (RFC 9172
// §5.1.1), and the save the drain staged is discarded: a payload tampered
// with in transit, and one whose BIB this node checks under another key.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_deletes_a_payload_that_fails_its_check() {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    for segments in [1, 3] {
        for (signed, encrypted, tamper) in [
            (false, true, true),
            (true, false, true),
            (true, false, false),
            (true, true, false),
        ] {
            let case = format!(
                "{segments} segments, signed {signed}, encrypted {encrypted}, tampered {tamper}"
            );
            let (inbound, _, mut keys) = with_protected_crcless_payload(LOCAL, signed, encrypted);
            let inbound = if tamper {
                let parsed = parse(inbound.clone()).unwrap();
                let data = parsed.bundle.blocks[&1].payload_range();
                let mut tampered = inbound.to_vec();
                tampered[usize::try_from(data.start).unwrap() + 100] ^= 0xFF;
                Bytes::from(tampered)
            } else {
                keys[0] = generated_keys().0;
                inbound
            };
            let (bpa, saved_rx, deleted_rx, sent_rx) =
                keyed_node_receiving_in(keys, None, inbound, segments).await;
            assert_eq!(
                deletion_reason(&sent_rx).await,
                ReasonCode::FailedSecurityOperation,
                "{case}"
            );
            bpa.shutdown().await;
            assert!(
                nothing_left_from(&saved_rx, &deleted_rx, &source),
                "{case}: any staged save is discarded"
            );
        }
    }
}

// A bundle from ipn:0.2.1 to node 1 whose source signed then encrypted its
// payload, as `with_protected_payload` does, and whose previous node,
// ipn:0.2.0, then encrypted the Previous Node block it added: the header
// pass, accepting the Previous Node, decrypts the encrypted BIB over the
// payload by necessity. The bundle and the keys.
#[cfg(feature = "rfc9173")]
fn with_protected_payload_and_previous_node() -> (Bytes, Vec<Key>) {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let previous: Eid = "ipn:0.2.0".parse().unwrap();
    let plaintext: Vec<u8> = (0..6144u32).map(|i| (i % 239) as u8).collect();
    let (_, data) = Builder::new(source.clone(), LOCAL.parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .add_extension_block(Type::PreviousNode)
        .unwrap()
        .build(emit(&previous).0.into())
        .with_payload(plaintext.as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let (sign_key, enc_key) = generated_keys();
    let parsed = parse(Bytes::from(data)).unwrap();
    let signed = parse(Bytes::from(
        Signer::new(&parsed.bundle, &parsed.data)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap(),
    ))
    .unwrap();
    let encrypted = parse(Bytes::from(
        Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(
                1,
                EncryptContext::AES_GCM(ScopeFlags::default()),
                source,
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap(),
    ))
    .unwrap();
    let previous_node = *encrypted
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::PreviousNode)
        .expect("the Previous Node is present")
        .0;
    let data = Encryptor::new(&encrypted.bundle, &encrypted.data)
        .encrypt_block(
            previous_node,
            EncryptContext::AES_GCM(ScopeFlags::default()),
            previous,
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();
    (Bytes::from(data), vec![sign_key, enc_key])
}

// A bundle from ipn:0.2.1 to node 1 whose source signed an extension block
// of type 199 and its 6 KiB payload into one BIB, in plaintext: the bundle,
// the block's number and the keys.
#[cfg(feature = "rfc9173")]
fn with_block_signed_with_the_payload() -> (Bytes, u64, Vec<Key>) {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let plaintext: Vec<u8> = (0..6144u32).map(|i| (i % 239) as u8).collect();
    let (built, data) = Builder::new(source.clone(), LOCAL.parse().unwrap())
        .add_extension_block(Type::Unrecognised(199))
        .unwrap()
        .build(b"signed".as_slice().into())
        .with_payload(plaintext.as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let block = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::Unrecognised(199))
        .expect("the block is present")
        .0;
    let (sign_key, enc_key) = generated_keys();
    // Without the security header in scope, the results share one BIB.
    let scope = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut signing = Signer::new(&built, &data);
    for n in [block, 1] {
        signing = signing
            .sign_block(
                n,
                signer::Context::HMAC_SHA2(scope.clone()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    (
        Bytes::from(signing.rebuild().expect("rebuild signed")),
        block,
        vec![sign_key, enc_key],
    )
}

// A block signed into one BIB with the payload is accepted with it at the
// destination, whether the payload arrives whole or in segments, which the
// header pass holds only a prefix of: the bundle is stored with the block
// and no BIB.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_accepts_a_block_signed_with_the_payload() {
    for segments in [1, 3] {
        let (inbound, block, keys) = with_block_signed_with_the_payload();
        let (bpa, saved_rx, _, _) = keyed_node_receiving_in(keys, None, inbound, segments).await;
        let stored = stored_from(&saved_rx, &"ipn:0.2.1".parse().unwrap()).await;
        assert!(stored.bibs.is_empty(), "{segments} segments");
        assert_eq!(
            stored.bundle.blocks[&block].payload(&stored.data),
            Some(b"signed".as_slice()),
            "{segments} segments"
        );
        bpa.shutdown().await;
    }
}

// The encrypted BIB over an encrypted payload, which the header pass
// decrypted by necessity, is checked against the payload's plaintext all
// the same: the bundle is stored with the payload in plaintext and no
// security block, and a BIB that fails under another key deletes it.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_local_destination_checks_a_payload_bib_the_header_pass_decrypted() {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    for segments in [1, 3] {
        let (inbound, keys) = with_protected_payload_and_previous_node();
        let (bpa, saved_rx, _, _) = keyed_node_receiving_in(keys, None, inbound, segments).await;
        let stored = stored_from(&saved_rx, &source).await;
        assert!(
            stored.bcbs.is_empty() && stored.bibs.is_empty(),
            "{segments} segments: every operation is accepted"
        );
        bpa.shutdown().await;

        let (inbound, mut keys) = with_protected_payload_and_previous_node();
        keys[0] = generated_keys().0;
        let (bpa, saved_rx, deleted_rx, sent_rx) =
            keyed_node_receiving_in(keys, None, inbound, segments).await;
        assert_eq!(
            deletion_reason(&sent_rx).await,
            ReasonCode::FailedSecurityOperation,
            "{segments} segments"
        );
        bpa.shutdown().await;
        assert!(
            nothing_left_from(&saved_rx, &deleted_rx, &source),
            "{segments} segments: any staged save is discarded"
        );
    }
}

// ---------------------------------------------------------------------------
// Fragments: security on a fragment is processed after reassembly (RFC 9172
// §5.2).
// ---------------------------------------------------------------------------

// `data` as fragment 0 of a bundle of 1 MiB, every block as it is.
#[cfg(feature = "rfc9173")]
fn as_fragment(data: Bytes) -> Bytes {
    let parsed = parse(data).unwrap();
    let chunks = Editor::new(&parsed.bundle, &parsed.data)
        .with_fragment_info(Some(FragmentInfo {
            offset: 0,
            total_adu_length: 1 << 20,
        }))
        .map_err(|(_, e)| e)
        .expect("set the fragment info")
        .rebuild()
        .expect("rebuild the fragment");
    Chunk::flatten_bytes(chunks, parsed.data)
}

// `data` fragmented `cut` bytes into its payload, every block in both
// fragments: the payload's operations ride in fragment 0, as RFC 9171 §5.8
// has them, and in fragment 1 as blocks replicated in every fragment.
#[cfg(feature = "rfc9173")]
fn fragments(data: &Bytes, cut: usize) -> (Bytes, Bytes) {
    let parsed = parse(data.clone()).unwrap();
    let payload = parsed.bundle.blocks[&1]
        .payload(&parsed.data)
        .expect("the payload is resident")
        .to_vec();
    let total_adu_length = payload.len() as u64;
    let fragment = |range: Range<usize>| {
        let chunks = Editor::new(&parsed.bundle, &parsed.data)
            .with_fragment_info(Some(FragmentInfo {
                offset: range.start as u64,
                total_adu_length,
            }))
            .map_err(|(_, e)| e)
            .expect("set the fragment info")
            .restore_block(1)
            .map_err(|(_, e)| e)
            .expect("restore the payload")
            .with_data(payload[range].to_vec().into())
            .rebuild()
            .rebuild()
            .expect("rebuild the fragment");
        Chunk::flatten_bytes(chunks, parsed.data.clone())
    };
    (fragment(0..cut), fragment(cut..payload.len()))
}

// The first whole bundle, no fragment, stored from `source`.
#[cfg(feature = "rfc9173")]
async fn stored_whole_from(saved_rx: &flume::Receiver<Bytes>, source: &Eid) -> Parsed {
    loop {
        let stored = stored_from(saved_rx, source).await;
        if stored.bundle.primary.id.fragment_info.is_none() {
            return stored;
        }
    }
}

// Metadata storage that reports every fragment parked for reassembly: each
// successful swap into `AduFragment`, by bundle id.
#[cfg(feature = "rfc9173")]
struct FragmentParks {
    inner: MetadataMemStorage,
    parked_tx: flume::Sender<Id>,
}

#[cfg(feature = "rfc9173")]
impl FragmentParks {
    fn new() -> (Arc<Self>, flume::Receiver<Id>) {
        let (parked_tx, parked_rx) = flume::unbounded();
        (
            Arc::new(Self {
                inner: MetadataMemStorage::new(None),
                parked_tx,
            }),
            parked_rx,
        )
    }
}

#[cfg(feature = "rfc9173")]
#[async_trait]
impl MetadataStorage for FragmentParks {
    async fn get(&self, bundle_id: &Id) -> StorageResult<Option<Bundle>> {
        self.inner.get(bundle_id).await
    }

    async fn insert(&self, bundle: &Bundle) -> StorageResult<bool> {
        self.inner.insert(bundle).await
    }

    async fn swap_status(
        &self,
        bundle_id: &Id,
        expected: &BundleStatus,
        status: &BundleStatus,
    ) -> StorageResult<bool> {
        let swapped = self.inner.swap_status(bundle_id, expected, status).await?;
        if swapped && matches!(status, BundleStatus::AduFragment { .. }) {
            let _ = self.parked_tx.send(bundle_id.clone());
        }
        Ok(swapped)
    }

    async fn tombstone_if(&self, bundle_id: &Id, expected: &BundleStatus) -> StorageResult<bool> {
        self.inner.tombstone_if(bundle_id, expected).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> StorageResult<()> {
        self.inner.tombstone(bundle_id).await
    }

    async fn start_recovery(&self) {
        self.inner.start_recovery().await;
    }

    async fn confirm_exists(
        &self,
        bundle_id: &Id,
    ) -> StorageResult<Option<(BundleMetadata, BundleStatus)>> {
        self.inner.confirm_exists(bundle_id).await
    }

    async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
        self.inner.remove_unconfirmed(stream).await
    }

    async fn reset_peer_queue(&self, peer: u32) -> StorageResult<u64> {
        self.inner.reset_peer_queue(peer).await
    }

    async fn reset_peer_ack_pending(&self, peer: u32) -> StorageResult<u64> {
        self.inner.reset_peer_ack_pending(peer).await
    }

    async fn reset_service_queue(&self, service: &Eid) -> StorageResult<u64> {
        self.inner.reset_service_queue(service).await
    }

    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
        self.inner.poll_expiry(stream).await
    }

    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
        self.inner.poll_waiting(stream).await
    }

    async fn poll_service_waiting(
        &self,
        source: Eid,
        stream: &dyn Sender<Bundle>,
    ) -> StorageResult<()> {
        self.inner.poll_service_waiting(source, stream).await
    }

    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> StorageResult<()> {
        self.inner.poll_adu_fragments(stream, status).await
    }

    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> StorageResult<()> {
        self.inner.poll_pending(stream, status, limit).await
    }
}

// The next fragment parked for reassembly.
#[cfg(feature = "rfc9173")]
async fn parked(parked_rx: &flume::Receiver<Id>) -> Id {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(Duration::from_secs(5), parked_rx.recv_async())
        .await
        .expect("timeout waiting for the park")
        .expect("channel closed")
}

// Decides whether this node reassembles every bundle it classifies.
#[cfg(feature = "rfc9173")]
struct Reassembles(Option<bool>);

#[cfg(feature = "rfc9173")]
impl Classifier for Reassembles {
    fn classify(&self, _ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        delta.reassemble = self.0;
        Verdict::Continue(delta)
    }
}

// A filter pack whose Classifier decides reassembly as `decision`.
#[cfg(feature = "rfc9173")]
fn reassembling(decision: Option<bool>) -> Option<FilterPack> {
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier("reassembles", Reassembles(decision));
    Some(pack)
}

// A bundle from ipn:0.2.1 to `destination` with a per-hop block of
// `block_type`, a Previous Node naming ipn:0.2.0, a Bundle Age of 5 s or a
// Hop Count of 1 of 64, which the previous node signed where `signed` and
// encrypted otherwise, under a scope that leaves the primary block out
// where `primary_excluded`, asking for deletion reports: the bundle and the
// key.
#[cfg(feature = "rfc9173")]
fn with_protected_per_hop_block(
    destination: &str,
    block_type: Type,
    signed: bool,
    primary_excluded: bool,
) -> (Bytes, Key) {
    let previous: Eid = "ipn:0.2.0".parse().unwrap();
    let scope = ScopeFlags {
        include_primary_block: !primary_excluded,
        ..ScopeFlags::default()
    };
    let body = if block_type == Type::PreviousNode {
        emit(&previous).0
    } else if block_type == Type::BundleAge {
        emit(&5_000u64).0
    } else {
        emit(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .0
    };
    let (built, data) = Builder::new("ipn:0.2.1".parse().unwrap(), destination.parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .add_extension_block(block_type)
        .unwrap()
        .build(body.into())
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let number = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block_type)
        .expect("the per-hop block is present")
        .0;
    let (sign_key, enc_key) = generated_keys();
    if signed {
        let signed = Signer::new(&built, &data)
            .sign_block(
                number,
                signer::Context::HMAC_SHA2(scope),
                previous,
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the block")
            .rebuild()
            .expect("rebuild the signed bundle");
        (Bytes::from(signed), sign_key)
    } else {
        let encrypted = Encryptor::new(&built, &data)
            .encrypt_block(number, EncryptContext::AES_GCM(scope), previous, &enc_key)
            .map_err(|(_, e)| e)
            .expect("encrypt the block")
            .rebuild()
            .expect("rebuild the encrypted bundle");
        (Bytes::from(encrypted), enc_key)
    }
}

// A bundle without a creation time from ipn:0.2.1 to a destination behind
// node 1's peer, carrying a Previous Node (ipn:0.2.0), a Bundle Age (5 s)
// and a Hop Count (1 of 64), none under an operation, and a 6 KiB payload
// its source signed in plaintext, asking for deletion reports: the bundle,
// the payload and the signing key.
#[cfg(feature = "rfc9173")]
fn with_per_hop_blocks_and_signed_payload() -> (Bytes, Vec<u8>, Key) {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let plaintext: Vec<u8> = (0..6144u32).map(|i| (i % 239) as u8).collect();
    let (built, data) = Builder::new(source.clone(), REMOTE.parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .add_extension_block(Type::PreviousNode)
        .unwrap()
        .build(emit(&"ipn:0.2.0".parse::<Eid>().unwrap()).0.into())
        .add_extension_block(Type::BundleAge)
        .unwrap()
        .build(emit(&5_000u64).0.into())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .with_payload(plaintext.as_slice().into())
        .build(CreationTimestamp::default())
        .unwrap();
    let (sign_key, _) = generated_keys();
    let signed = Signer::new(&built, &data)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            source,
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("sign the payload")
        .rebuild()
        .expect("rebuild the signed bundle");
    (Bytes::from(signed), plaintext, sign_key)
}

// A bundle without a creation time from ipn:0.9.1 to a local service, its
// Bundle Age (5 s) under no operation and its 6 KiB payload in plaintext,
// reporting to ipn:0.2.1: the bundle and the payload.
#[cfg(feature = "rfc9173")]
fn unclocked_with_bundle_age() -> (Bytes, Vec<u8>) {
    let plaintext: Vec<u8> = (0..6144u32).map(|i| (i % 239) as u8).collect();
    let (_, data) = Builder::new("ipn:0.9.1".parse().unwrap(), LOCAL.parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:0.2.1".parse().unwrap())
        .add_extension_block(Type::BundleAge)
        .unwrap()
        .build(emit(&5_000u64).0.into())
        .with_payload(plaintext.as_slice().into())
        .build(CreationTimestamp::default())
        .unwrap();
    (Bytes::from(data), plaintext)
}

// A BIB over a fragment's payload is checked after reassembly, never
// against the fragment's partial payload: fragment 0 of a bundle whose
// payload is signed in plaintext carries that BIB (RFC 9171 §5.8), and a
// relay forwards the fragment as received, whether it arrives whole or in
// segments.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_leaves_a_fragments_plaintext_payload_bib_unchecked() {
    let (inbound, _, keys) = with_protected_payload(REMOTE, true, false);
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let fragment = as_fragment(inbound);

    for segments in [1, 3] {
        let (bpa, saved_rx, _, sent_rx) =
            keyed_node_receiving_in(keys.clone(), None, fragment.clone(), segments).await;
        let stored = stored_from(&saved_rx, &source).await;
        assert_eq!(
            stored.data, fragment,
            "{segments} segments: stored as received"
        );
        let forwarded = forwarded_from(&sent_rx, &source).await;
        assert_eq!(forwarded.bundle.primary.id, stored.bundle.primary.id);
        bpa.shutdown().await;
    }
}

// The destination reassembles the fragments of a bundle whose payload its
// source signed in plaintext, and accepts the operation over the whole (RFC
// 9172 §5.2): the reassembled bundle keeps fragment 0's BIB over the
// restored payload, and crosses the door to be stored in plaintext without
// it, the check passed. Fragment 1 is received once fragment 0 is parked
// for reassembly.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_destination_verifies_a_signed_payload_once_reassembled() {
    let (inbound, plaintext, keys) = with_protected_payload(LOCAL, true, false);
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let (first, second) = fragments(&inbound, 4000);
    let (parks, parked_rx) = FragmentParks::new();
    let (bpa, cla, saved_rx, _, _) = keyed_node(keys, None, Some(parks)).await;

    receive_in(&cla, first.clone(), 1).await;
    assert_eq!(
        stored_from(&saved_rx, &source).await.data,
        first,
        "fragment 0 is stored as received"
    );
    assert!(parked(&parked_rx).await.fragment_info.is_some());
    receive_in(&cla, second, 3).await;

    let reassembled = stored_whole_from(&saved_rx, &source).await;
    assert_eq!(
        reassembled.bundle.blocks[&1].payload(&reassembled.data),
        Some(plaintext.as_slice())
    );
    assert_eq!(
        reassembled.bibs.len(),
        1,
        "the whole carries fragment 0's BIB over the payload"
    );
    let accepted = stored_whole_from(&saved_rx, &source).await;
    assert_eq!(
        accepted.bundle.blocks[&1].payload(&accepted.data),
        Some(plaintext.as_slice())
    );
    assert!(
        accepted.bibs.is_empty() && accepted.bcbs.is_empty(),
        "the destination accepted the whole's operation"
    );
    bpa.shutdown().await;
}

// A fragment tampered in transit fails the check the destination makes over
// the reassembled whole: the bundle is deleted at the door it re-enters,
// "Failed security operation", and nothing of it is left in storage.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_tampered_fragment_fails_the_check_over_the_reassembled_payload() {
    let (inbound, _, keys) = with_protected_crcless_payload(LOCAL, true, false);
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let (first, second) = fragments(&inbound, 4000);
    // The last byte of the second fragment's payload: the value changes,
    // and the fragment, carrying no CRC, still parses.
    let tampered = {
        let parsed = parse(second).unwrap();
        let mut data = parsed.data.to_vec();
        let last = usize::try_from(parsed.bundle.blocks[&1].payload_range().end).unwrap() - 1;
        data[last] ^= 0x01;
        Bytes::from(data)
    };
    let (parks, parked_rx) = FragmentParks::new();
    let (bpa, cla, saved_rx, deleted_rx, sent_rx) = keyed_node(keys, None, Some(parks)).await;

    receive_in(&cla, first, 1).await;
    parked(&parked_rx).await;
    receive_in(&cla, tampered, 1).await;

    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::FailedSecurityOperation
    );
    bpa.shutdown().await;
    assert!(nothing_left_from(&saved_rx, &deleted_rx, &source));
}

// The destination reassembles the fragments of a bundle whose payload its
// source signed and then encrypted, the BIB under the same encryption: the
// reassembled bundle keeps fragment 0's BCB and BIB over the restored
// ciphertext, and the destination decrypts and verifies the whole, storing
// it in plaintext without them.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_destination_decrypts_an_encrypted_payload_once_reassembled() {
    let (inbound, plaintext, keys) = with_protected_payload(LOCAL, true, true);
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let (first, second) = fragments(&inbound, 4000);
    let (parks, parked_rx) = FragmentParks::new();
    let (bpa, cla, saved_rx, _, _) = keyed_node(keys, None, Some(parks)).await;

    receive_in(&cla, first, 1).await;
    parked(&parked_rx).await;
    receive_in(&cla, second, 3).await;

    let reassembled = stored_whole_from(&saved_rx, &source).await;
    assert!(
        !reassembled.bcbs.is_empty() && reassembled.bundle.blocks[&1].bcb.is_some(),
        "the whole carries fragment 0's BCB over the payload"
    );
    assert_ne!(
        reassembled.bundle.blocks[&1].payload(&reassembled.data),
        Some(plaintext.as_slice()),
        "the restored payload is the ciphertext"
    );
    let accepted = stored_whole_from(&saved_rx, &source).await;
    assert_eq!(
        accepted.bundle.blocks[&1].payload(&accepted.data),
        Some(plaintext.as_slice())
    );
    assert!(accepted.bibs.is_empty() && accepted.bcbs.is_empty());
    bpa.shutdown().await;
}

// A fragment's per-hop blocks are processed as any bundle's (RFC 9171
// §5.4), being the forwarder's writes to the fragment: the header pass
// accepts the operation over one, computed over the fragment as one whose
// scope leaves the primary block out is, and the fragment is stored with
// the block in plaintext, its value as received, and no security block,
// then forwarded as a fragment. A Previous Node whose operation this node
// cannot accept, holding no key for it, or whose signature covers the
// primary block as it was before fragmentation, is a failed acceptance on
// a fragment as on any bundle: discarded, as its flags decide.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_fragments_per_hop_blocks_are_accepted_as_any_bundles() {
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    for (block_type, signed, primary_excluded, held) in [
        (Type::PreviousNode, true, true, true),
        (Type::BundleAge, false, true, true),
        (Type::HopCount, true, true, true),
        (Type::HopCount, false, true, true),
        (Type::PreviousNode, true, true, false),
        (Type::PreviousNode, true, false, true),
    ] {
        let accepted = held && primary_excluded;
        let case = format!(
            "{block_type:?} signed {signed}, primary excluded {primary_excluded}, key held {held}"
        );
        let (inbound, key) =
            with_protected_per_hop_block(REMOTE, block_type, signed, primary_excluded);
        let fragment = as_fragment(inbound);
        let keys = if held { vec![key] } else { Vec::new() };
        let (bpa, saved_rx, sent_rx) = keyed_node_receiving(keys, None, fragment).await;

        let stored = stored_from(&saved_rx, &source).await;
        assert!(stored.bundle.primary.id.fragment_info.is_some(), "{case}");
        assert!(stored.bibs.is_empty() && stored.bcbs.is_empty(), "{case}");
        let block = stored
            .bundle
            .blocks
            .values()
            .find(|b| b.block_type == block_type);
        if accepted {
            let value = block
                .expect("the accepted block stays")
                .payload(&stored.data)
                .expect("the block is resident");
            let received: Vec<u8> = if block_type == Type::PreviousNode {
                emit(&"ipn:0.2.0".parse::<Eid>().unwrap()).0
            } else if block_type == Type::BundleAge {
                emit(&5_000u64).0
            } else {
                emit(&HopInfo {
                    limit: NonZeroU8::new(64).unwrap(),
                    count: 1,
                })
                .0
            };
            assert_eq!(value, received.as_slice(), "{case}");
        } else {
            assert!(block.is_none(), "{case}: the block is discarded");
        }
        let forwarded = forwarded_from(&sent_rx, &source).await;
        assert_eq!(
            forwarded.bundle.primary.id, stored.bundle.primary.id,
            "{case}"
        );
        bpa.shutdown().await;
    }
}

// A transit fragment is forwarded as any bundle is: its Previous Node
// replaced, its Bundle Age advanced by its dwell here and its Hop Count
// incremented (RFC 9171 §5.4), none being under an operation, and the
// operation over its payload untouched, for the node that reassembles the
// bundle to check.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_relay_forwards_a_fragment_with_its_per_hop_blocks_replaced() {
    let (inbound, plaintext, key) = with_per_hop_blocks_and_signed_payload();
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let fragment = as_fragment(inbound);
    let (bpa, saved_rx, sent_rx) = keyed_node_receiving(vec![key], None, fragment.clone()).await;

    assert_eq!(
        stored_from(&saved_rx, &source).await.data,
        fragment,
        "stored as received"
    );
    let forwarded = forwarded_from(&sent_rx, &source).await;
    assert!(forwarded.bundle.primary.id.fragment_info.is_some());
    let block = |block_type: Type| {
        forwarded
            .bundle
            .blocks
            .values()
            .find(|b| b.block_type == block_type)
            .expect("the block is present")
            .payload(&forwarded.data)
            .expect("the block is resident")
    };
    let previous_node: Eid =
        hardy_cbor::decode::parse(block(Type::PreviousNode)).expect("a node ID");
    assert_eq!(
        previous_node,
        "ipn:0.1.0".parse().unwrap(),
        "the Previous Node names this node"
    );
    let age: u64 = hardy_cbor::decode::parse(block(Type::BundleAge)).expect("an age");
    assert!(age >= 5_000, "the Bundle Age advanced: {age}");
    let hops: HopInfo = hardy_cbor::decode::parse(block(Type::HopCount)).expect("a hop count");
    assert_eq!(hops.count, 2, "the Hop Count incremented");
    assert_eq!(
        forwarded.bundle.blocks[&1].payload(&forwarded.data),
        Some(plaintext.as_slice())
    );
    assert!(
        matches!(forwarded.bundle.blocks[&1].bib, BibCoverage::Some(_)),
        "the payload's BIB travels"
    );
    bpa.shutdown().await;
}

// A fragment without a creation time passes the lifetime gate on its Bundle
// Age, under no operation, to reassembly: the whole, with fragment 0's
// Bundle Age as received, is stored.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn an_unclocked_fragment_passes_the_gate_to_reassembly() {
    let (inbound, plaintext) = unclocked_with_bundle_age();
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let (first, second) = fragments(&inbound, 4000);
    let (parks, parked_rx) = FragmentParks::new();
    let (bpa, cla, saved_rx, _, _) = keyed_node(Vec::new(), None, Some(parks)).await;

    receive_in(&cla, first, 1).await;
    parked(&parked_rx).await;
    receive_in(&cla, second, 1).await;

    let reassembled = stored_whole_from(&saved_rx, &source).await;
    assert!(!reassembled.bundle.primary.id.timestamp.is_clocked());
    assert!(reassembled.per_hop.bundle_age.is_some());
    assert_eq!(
        reassembled.bundle.blocks[&1].payload(&reassembled.data),
        Some(plaintext.as_slice())
    );
    bpa.shutdown().await;
}

// An ADU whose total length exceeds the size cap is never rebuilt: the
// whole would be materialised in memory and then refused by the cap as it
// re-entered the door. Each fragment is dropped at reassembly as it
// arrives, "Depleted storage", reported, and nothing whole is stored.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_fragment_of_an_adu_over_the_size_cap_is_dropped() {
    let (inbound, _) = unclocked_with_bundle_age();
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    // Each fragment is under the cap; the 6 KiB ADU is over it.
    let (first, second) = fragments(&inbound, 3072);
    let (bpa, cla, saved_rx, _, sent_rx) =
        keyed_node_under(NonZeroU64::new(4096), Vec::new(), None, None).await;

    receive_in(&cla, first, 1).await;
    let status = sent_status_report(&sent_rx).await;
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::DepletedStorage);

    receive_in(&cla, second, 1).await;
    let status = sent_status_report(&sent_rx).await;
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::DepletedStorage);

    bpa.shutdown().await;
    assert!(
        saved_rx.try_iter().all(|data| {
            let stored = parse(data).expect("a stored bundle parses");
            stored.bundle.primary.id.source != source
                || stored.bundle.primary.id.fragment_info.is_some()
        }),
        "nothing whole is stored"
    );
}

// A Classifier claims a transit fragment set for reassembly here
// (`MetadataDelta::reassemble`): the fragments are held, and the whole is
// rebuilt and forwarded as routed, the operation over its payload untouched.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_classifier_claims_a_transit_fragment_set_for_reassembly() {
    let (inbound, plaintext, keys) = with_protected_payload(REMOTE, true, false);
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let (first, second) = fragments(&inbound, 4000);
    let (parks, parked_rx) = FragmentParks::new();
    let (bpa, cla, _, _, sent_rx) = keyed_node(keys, reassembling(Some(true)), Some(parks)).await;

    receive_in(&cla, first, 1).await;
    parked(&parked_rx).await;
    receive_in(&cla, second, 1).await;

    let forwarded = forwarded_from(&sent_rx, &source).await;
    assert!(
        forwarded.bundle.primary.id.fragment_info.is_none(),
        "the whole is forwarded"
    );
    assert_eq!(
        forwarded.bundle.blocks[&1].payload(&forwarded.data),
        Some(plaintext.as_slice())
    );
    assert_eq!(forwarded.bibs.len(), 1, "a relay accepts nothing");
    bpa.shutdown().await;
}

// A Classifier declines to reassemble a fragment addressed here: it is
// dropped at the gate and reported, and nothing is stored, since a raw
// fragment is never delivered.
#[cfg(feature = "rfc9173")]
#[tokio::test]
async fn a_classifier_declines_a_fragment_addressed_here() {
    let (inbound, _, keys) = with_protected_payload(LOCAL, true, false);
    let fragment = as_fragment(inbound);
    let (bpa, saved_rx, sent_rx) =
        keyed_node_receiving(keys, reassembling(Some(false)), fragment).await;
    assert_eq!(
        deletion_reason(&sent_rx).await,
        ReasonCode::NoAdditionalInformation
    );
    bpa.shutdown().await;
    assert_never_stored(&saved_rx, &"ipn:0.2.1".parse().unwrap());
}
