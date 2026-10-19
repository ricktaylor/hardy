/*!
A custodian for the Custody Receipt Block (draft-bsvkey-dtn-custody-receipt-block-01): an Egress [`Rewriter`] that signs a version 2 custody receipt for every bundle this node forwards and appends it to the bundle's Custody Receipt Block (experimental block type 224), adding the block when it is absent.

The receipt binds this node's EID, the node it received the bundle from, the BPv7 bundle identity, a SHA-256 of the payload block's data as held, and the reception and forwarding times, signed as a COSE_Sign1 with EdDSA. The Rewriter edits only block 224 through the scoped [`ExtensionEditor`](hardy_bpv7::extension_editor::ExtensionEditor): it never touches the payload or any other block, and it reads the payload only to hash the bytes held. Earlier receipts are copied unchanged.

A node that does not run this filter forwards the block unchanged (draft Section 4.2), so custodians and plain forwarders mix on one path.

Register the filter with [`CustodyReceipt::filter_pack`] and
[`BpaBuilder::add_filters`](hardy_bpa::builder::BpaBuilder::add_filters).
*/

use std::{fmt, path::PathBuf};

use ed25519_dalek::{SECRET_KEY_LENGTH, SigningKey, VerifyingKey};
use hardy_bpa::{
    bundle::Origin,
    filter::{Boundary, RewriteContext, Rewriter, pack::FilterPack},
};
use hardy_bpv7::{
    block::{Flags, Type},
    crc::CrcType,
    dtn_time::DtnTime,
    eid::{Eid, NodeId},
    reader::Availability,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{debug, warn};
use zeroize::Zeroizing;

pub mod receipt;

use crate::receipt::{BundleIdentity, Fields};

/// Errors from building a [`CustodyReceipt`] from its configuration.
#[derive(Debug, Error)]
pub enum Error {
    /// The signing-key file could not be read.
    #[error("cannot read the custody signing key file {path}: {source}")]
    KeyFile {
        /// The file named in the configuration.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The signing-key file does not hold a 32-byte Ed25519 seed as 64
    /// hexadecimal digits.
    #[error("the custody signing key file {0} does not hold a 32-byte Ed25519 seed in hexadecimal")]
    KeyFormat(PathBuf),
}

pub type Result<T> = core::result::Result<T, Error>;

/// Which forwarded bundles this node signs for (draft Section 5.1: local policy).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum Policy {
    /// Sign every forwarded bundle that is not an administrative record,
    /// adding the block where it is absent.
    #[default]
    Always,
    /// Sign only bundles that already carry a Custody Receipt Block.
    BlockPresent,
}

/// The `custody-receipt` configuration section.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(
    feature = "serde",
    serde(deny_unknown_fields, rename_all = "kebab-case")
)]
pub struct Config {
    /// This custodian's EID, the receipts' thisHop, as verifiers pin it.
    pub eid: String,
    /// A file holding the Ed25519 signing seed as 64 hexadecimal digits.
    pub key_file: PathBuf,
    /// Which forwarded bundles to sign for.
    #[cfg_attr(feature = "serde", serde(default))]
    pub policy: Policy,
}

/// The custodian Rewriter.
pub struct CustodyReceipt {
    this_hop: String,
    key: SigningKey,
    policy: Policy,
}

// The signing key never reaches `Debug`.
impl fmt::Debug for CustodyReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustodyReceipt")
            .field("this_hop", &self.this_hop)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl CustodyReceipt {
    /// A custodian signing as `this_hop` with `key`.
    #[must_use]
    pub fn new(this_hop: String, key: SigningKey, policy: Policy) -> Self {
        Self {
            this_hop,
            key,
            policy,
        }
    }

    /// A custodian built from its configuration section, reading the
    /// signing seed from the key file.
    ///
    /// # Errors
    ///
    /// [`Error::KeyFile`] or [`Error::KeyFormat`].
    pub fn from_config(config: &Config) -> Result<Self> {
        let text = Zeroizing::new(std::fs::read_to_string(&config.key_file).map_err(|source| {
            Error::KeyFile {
                path: config.key_file.clone(),
                source,
            }
        })?);
        let seed =
            parse_seed(text.trim()).ok_or_else(|| Error::KeyFormat(config.key_file.clone()))?;
        Ok(Self::new(
            config.eid.clone(),
            SigningKey::from_bytes(&seed),
            config.policy,
        ))
    }

    /// The public key verifiers pin for this custodian.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// This custodian's EID.
    #[must_use]
    pub fn this_hop(&self) -> &str {
        &self.this_hop
    }

    /// A filter pack registering this custodian at the Egress hook.
    pub fn filter_pack(self) -> FilterPack {
        let mut pack = FilterPack::new("custody-receipt");
        pack.egress_rewriter("stamp", self);
        pack
    }
}

// 64 hexadecimal digits to a 32-byte seed, held in zeroizing storage.
fn parse_seed(hex: &str) -> Option<Zeroizing<[u8; SECRET_KEY_LENGTH]>> {
    let digits = hex.as_bytes();
    if digits.len() != SECRET_KEY_LENGTH * 2 {
        return None;
    }
    let mut seed = Zeroizing::new([0u8; SECRET_KEY_LENGTH]);
    for (byte, pair) in seed.iter_mut().zip(digits.as_chunks::<2>().0) {
        let hi = char::from(pair[0]).to_digit(16)?;
        let lo = char::from(pair[1]).to_digit(16)?;
        *byte = u8::try_from(hi << 4 | lo).ok()?;
    }
    Some(seed)
}

// A node ID in the EID text form receipts carry (`ipn:N.0`, `dtn://node/`).
fn node_eid(node: &NodeId) -> Option<String> {
    match node {
        NodeId::Ipn(fqnn) => Some(
            Eid::Ipn {
                fqnn: *fqnn,
                service_number: 0,
            }
            .to_string(),
        ),
        NodeId::Dtn(node_name) => Some(
            Eid::Dtn {
                node_name: node_name.clone(),
                service_name: "".into(),
            }
            .to_string(),
        ),
        NodeId::LocalNode => None,
    }
}

impl Rewriter for CustodyReceipt {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        if !matches!(ctx.boundary(), Boundary::Egress { .. }) {
            return;
        }
        let bundle = ctx.bundle();
        let reader = ctx.reader();
        let metadata = ctx.metadata();
        let id = &bundle.primary.id;
        if bundle.primary.flags.is_admin_record {
            return;
        }

        // At most one Custody Receipt Block (draft Section 4.1); its data as held.
        let mut existing = bundle
            .blocks
            .iter()
            .filter(|(_, block)| block.block_type == Type::Unrecognised(receipt::BLOCK_TYPE));
        let current = existing.next().map(|(number, _)| *number);
        if existing.next().is_some() {
            warn!(bundle = %id.source, "more than one Custody Receipt Block; not signing");
            return;
        }
        let current = match current {
            Some(number) => match reader.block(number) {
                Some((_, Availability::Available(data))) => Some((number, data)),
                _ => {
                    debug!("Custody Receipt Block not readable here; not signing");
                    return;
                }
            },
            None if self.policy == Policy::BlockPresent => return,
            None => None,
        };

        // The payload block's data exactly as held: an encrypted payload is
        // hashed as its ciphertext, which this reader would decrypt, so it is
        // left unsigned rather than hashed as plaintext.
        let Some(payload_block) = bundle.blocks.get(&1) else {
            return;
        };
        if payload_block.bcb.is_some() {
            debug!("payload is BCB-covered; not signing");
            return;
        }
        let payload_hash: [u8; 32] = match reader.block(1) {
            Some((_, Availability::Available(payload))) => Sha256::digest(payload.as_ref()).into(),
            _ => {
                debug!("payload not resident; not signing");
                return;
            }
        };

        // The previous hop: the Previous Node block when there is one
        // (draft Section 3), else the CLA's ingress peer, else, for a
        // bundle sourced here, its source.
        let source = id.source.to_string();
        let prev_hop = match (&metadata.extensions.previous_node, metadata.origin()) {
            (Some(previous), _) => previous.to_string(),
            (
                None,
                Origin::Ingress {
                    peer_node: Some(peer),
                    ..
                },
            ) => match node_eid(peer) {
                Some(eid) => eid,
                None => return,
            },
            (None, Origin::Originated) => source.clone(),
            (None, _) => {
                debug!("previous hop unknown; not signing");
                return;
            }
        };

        let (creation_time, sequence_number) = id.timestamp.clone().into_parts();
        let fields = Fields {
            prev_hop: &prev_hop,
            this_hop: &self.this_hop,
            bundle: BundleIdentity {
                source: &source,
                creation_time: creation_time.map_or(0, |t| t.millisecs()),
                sequence_number,
                fragment: id
                    .fragment_info
                    .as_ref()
                    .map(|f| (f.offset, f.total_adu_length)),
            },
            payload_hash,
            received_at: DtnTime::saturating_from(metadata.received_at()).millisecs(),
            forwarded_at: DtnTime::now().millisecs(),
            subject: None,
        };
        let signed = receipt::sign(&self.key, &fields.encode());

        let edit = match current {
            Some((number, data)) => match receipt::append(data.as_ref(), &signed) {
                Ok(data) => ctx.editor().replace(number, data.into()),
                Err(e) => {
                    debug!("Custody Receipt Block not appendable: {e}");
                    return;
                }
            },
            None => ctx
                .editor()
                .insert(
                    Type::Unrecognised(receipt::BLOCK_TYPE),
                    Flags {
                        must_replicate: true,
                        ..Default::default()
                    },
                    CrcType::CRC32_CASTAGNOLI,
                    receipt::new_block(&signed).into(),
                )
                .map(|_| ()),
        };
        // A refusal (for example a block under BPSec coverage) is the no-match path.
        if let Err(e) = edit {
            debug!("Custody Receipt Block edit refused: {e}");
        }
    }
}
