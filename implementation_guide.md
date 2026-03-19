# BP-ARP Implementation Guide

This guide details the steps required to update the BP-ARP implementation in `hardy` to align with the latest RFC draft and introduce a feature flag.

## Overview of Changes

1. **Feature Flagging**: Introduce a `bp-arp` feature in both `bpv7` and `bpa` crates.
2. **Administrative Record**: Replace `BpArpProbe` (type 2) and `BpArpAck` (type 3) with a single `BpArp` variant (type 2).
3. **Protocol Logic**: Distinguish BP-ARP Requests (Probes) from Responses (Acks) based on the destination EID.
    - **Request**: Destination is `ipn:!.0` (LocalNode).
    - **Response**: Destination is a specific Node ID.

---

## Step 1: Update `bpv7` Crate

### 1.1 Add Feature Flag to `bpv7/Cargo.toml`

Add the `bp-arp` feature to the `[features]` section.

```toml
[features]
default = ["rfc9173"]
# ... existing features ...
bp-arp = []
```

### 1.2 Refactor `AdministrativeRecord` in `bpv7/src/status_report.rs`

Modify the `AdministrativeRecord` enum to use a single `BpArp` variant, gated behind the `bp-arp` feature.

**Remove:**

```rust
    /// A BP-ARP probe (type 2). Sent to `ipn:!.0` (LocalNode) to solicit an EID reply.
    BpArpProbe,
    /// A BP-ARP acknowledgement (type 3). Sent in response to a `BpArpProbe`.
    BpArpAck(Vec<crate::eid::Eid>),
```

**Add:**

```rust
    /// A BP-ARP record (type 2).
    /// - If destination is `ipn:!.0`, it is a Request (Probe).
    /// - If destination is a Node ID, it is a Response (Ack).
    #[cfg(feature = "bp-arp")]
    BpArp(Vec<crate::eid::Eid>),
```

**Update `ToCbor` implementation:**

```rust
impl hardy_cbor::encode::ToCbor for AdministrativeRecord {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        match self {
            AdministrativeRecord::BundleStatusReport(report) => encoder.emit(&(1u64, report)),

            #[cfg(feature = "bp-arp")]
            AdministrativeRecord::BpArp(eids) => encoder.emit_array(Some(2), |a| {
                a.emit(&2u64); // Type 2 (TBD1)
                a.emit(eids.as_slice());
            }),
        }
    }
}
```

**Update `FromCbor` implementation:**

```rust
impl hardy_cbor::decode::FromCbor for AdministrativeRecord {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, mut shortest, tags| {
            // ... existing setup ...

            match a.parse()... {
                1u64 => { ... },

                #[cfg(feature = "bp-arp")]
                2u64 => {
                     let (eids, s) = a.parse_array(|a, s, _tags| {
                        let mut eids = Vec::new();
                        while let Some(eid) = a
                            .try_parse::<crate::eid::Eid>()
                            .map_field_err::<Error>("BpArp eid")?
                        {
                            eids.push(eid);
                        }
                        Ok::<_, Error>((eids, s))
                    }).map_field_err::<Error>("BpArp payload")?;
                    Ok((Self::BpArp(eids), shortest && s))
                },

                v => Err(Error::UnknownAdminRecordType(v)),
            }
        })
        .map(...)
    }
}
```

---

## Step 2: Update `bpa` Crate Configuration and Logic

### 2.1 Add Feature Flag to `bpa/Cargo.toml`

Add `bp-arp` feature and propagate it to `hardy-bpv7`.

```toml
[features]
# ... existing features ...
bp-arp = ["hardy-bpv7/bp-arp"]
```

### 2.2 Update `ArpSubsystem` in `bpa/src/cla/arp.rs`

To support the feature flag without breaking the `Registry` (which depends on `ArpSubsystem` and `ArpConfig`), we will provide a **stub implementation** when `bp-arp` is disabled. This avoids the need to gate fields in `Config` and `Registry`, keeping those files clean.

Wrap the existing code in a module or `cfg` block, and add a stub for the disabled state.

**Structure of `bpa/src/cla/arp.rs`:**

```rust
use super::*;
// ... other imports ...

// ============================================================================
// ENABLED: Real Implementation
// ============================================================================
#[cfg(feature = "bp-arp")]
pub use enabled::*;

#[cfg(feature = "bp-arp")]
mod enabled {
    use super::*;
    use hardy_bpv7::{
        bundle::Flags, creation_timestamp::CreationTimestamp, eid::Eid, hop_info::HopInfo,
        status_report::AdministrativeRecord,
    };
    use trace_err::*;

    // ... MOVE ALL EXISTING STRUCTS (ArpConfig, ArpPolicy, ArpSubsystem) HERE ...

    // ... Update build_probe and build_ack as described below ...

    impl ArpSubsystem {
        // ... existing methods ...

        fn build_probe(&self) -> Bytes {
             // ...
             let payload = hardy_cbor::encode::emit(&AdministrativeRecord::BpArp(Vec::new())).0;
             // ...
        }

        pub fn build_ack(&self, destination: &Eid) -> Bytes {
             // ...
             let payload = hardy_cbor::encode::emit(&AdministrativeRecord::BpArp(eids)).0;
             // ...
        }
    }
}

// ============================================================================
// DISABLED: Stub Implementation
// ============================================================================
#[cfg(not(feature = "bp-arp"))]
pub use disabled::*;

#[cfg(not(feature = "bp-arp"))]
mod disabled {
    use super::*;

    /// Stub ArpConfig (empty) to satisfy Config struct requirements
    #[derive(Debug, Clone, Default)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct ArpConfig;

    /// Stub ArpPolicy (empty)
    #[derive(Debug, Clone, Default)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct ArpPolicy;

    /// Stub ArpSubsystem (no-op)
    pub struct ArpSubsystem;

    impl ArpSubsystem {
        pub fn new(_config: ArpConfig, _node_ids: Arc<node_ids::NodeIds>) -> Arc<Self> {
            Arc::new(Self)
        }

        pub async fn on_neighbour_added(
            self: &Arc<Self>,
            _peer_id: u32,
            _cla: Arc<registry::Cla>,
            _cla_addr: ClaAddress,
            _tasks: &hardy_async::TaskPool,
        ) {}

        pub async fn on_neighbour_removed(&self, _peer_id: u32) {}

        pub async fn on_ack_received(&self, _peer_id: u32) {}
    }
}
```

### 2.3 Update `Dispatcher` in `bpa/src/dispatcher/admin.rs`

Handle the `BpArp` record type and implement the request/response distinction logic.

```rust
            #[cfg(feature = "bp-arp")]
            Ok(AdministrativeRecord::BpArp(eids)) => {
                // Check if this is a Request (Probe) or Response (Ack) based on Destination EID
                // Request: Destination is ipn:!.0 (LocalNode)
                if matches!(bundle.bundle.id.destination, hardy_bpv7::eid::Eid::LocalNode(0)) {
                    // It's a Probe (Request)
                    debug!("Received BP-ARP probe from {}", bundle.bundle.id.source);
                    let source = bundle.bundle.id.source.clone();

                    // Send a Response (Ack) back with our EIDs
                    let ack_payload = hardy_cbor::encode::emit(&AdministrativeRecord::BpArp(
                        self.cla_registry.all_admin_endpoints(),
                    ))
                    .0;
                    self.dispatch_admin_bundle(ack_payload, &source).await;
                } else {
                    // It's an Ack (Response)
                    debug!("Received BP-ARP response from {}", bundle.bundle.id.source);

                    // Promote neighbour
                    let mut all_eids = eids;
                    let source = bundle.bundle.id.source.clone();
                    if !all_eids.contains(&source) {
                        all_eids.push(source);
                    }

                    if let Some(peer_addr) = bundle.metadata.read_only.ingress_peer_addr.clone() {
                        self.cla_registry
                            .promote_neighbour(&peer_addr, all_eids)
                            .await;
                    } else {
                        debug!("BP-ARP response received without ingress peer address, cannot promote");
                    }
                }
                self.drop_bundle(bundle, None).await;
            }
```

## Step 3: Verification

1. Run `cargo check` (default features, bp-arp disabled) -> Should pass.
2. Run `cargo check --features bp-arp` -> Should pass.
3. Run `cargo fmt` to ensure formatting.
4. Run `cargo clippy` to check for lints.
