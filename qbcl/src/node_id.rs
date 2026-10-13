//! Peer Node ID authentication from TLS certificates (Section 4.2).
//!
//! Qubicle exchanges no Node IDs in-band: a peer's Node ID is the one its certificate authenticates through a NODE-ID, a subjectAltName otherName of form BundleEID whose value is a node ID (RFC 9174 Section 4.4.1).

use hardy_bpv7::eid::NodeId;
use thiserror::Error;
use x509_parser::{
    // Aliased: this module's own error type is `Error`.
    asn1_rs::{Error as Asn1Error, FromDer, Ia5String, TaggedExplicit},
    error::X509Error,
    extensions::GeneralName,
    parse_x509_certificate,
};

/// A specialized `Result` type for Node ID authentication.
pub type Result<T> = core::result::Result<T, Error>;

/// A failure to authenticate a peer's Node ID, which fails the TLS handshake as a certificate validation failure (Section 4.2).
#[derive(Debug, Error)]
pub enum Error {
    /// The end-entity certificate could not be parsed.
    #[error("the certificate could not be parsed: {0}")]
    Certificate(#[source] X509Error),

    /// The certificate has no NODE-ID, and no Node ID is configured for the peer.
    #[error("the certificate has no NODE-ID and no Node ID is configured for the peer")]
    Absent,

    /// The certificate's NODE-IDs do not include the Node ID configured for the peer.
    #[error(
        "the certificate's NODE-IDs {presented:?} do not include the configured Node ID {configured}"
    )]
    Mismatch {
        /// The Node ID configured for the peer.
        configured: NodeId,
        /// The node IDs the certificate's NODE-IDs carry.
        presented: Vec<NodeId>,
    },
}

/// The DER content octets of the id-on-bundleEID object identifier, 1.3.6.1.5.5.7.8.11 (RFC 9174 Section 4.4.2.1 and Appendix B).
const ID_ON_BUNDLE_EID: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x08, 0x0b];

/// The node IDs that the NODE-IDs of a DER-encoded certificate authenticate, in certificate order.
///
/// A BundleEID value is an IA5String URI (RFC 9174 Section 4.4.2.1). An entry whose value is any other URI, including an endpoint ID that is not a node ID, is ignored (RFC 9174 Section 4.4.1), as is one whose value is not an IA5String URI, and the local-node sentinel, which names no remote node.
///
/// # Errors
///
/// [`Error::Certificate`] if the certificate, or its subjectAltName extension, cannot be parsed.
pub fn node_ids(der: &[u8]) -> Result<Vec<NodeId>> {
    let (_, cert) = parse_x509_certificate(der).map_err(|e| Error::Certificate(e.into()))?;
    let Some(san) = cert
        .subject_alternative_name()
        .map_err(Error::Certificate)?
    else {
        return Ok(Vec::new());
    };
    Ok(san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::OtherName(oid, value) if oid.as_bytes() == ID_ON_BUNDLE_EID => {
                bundle_eid_node_id(value)
            }
            _ => None,
        })
        .collect())
}

/// Authenticates a peer's Node IDs from its DER-encoded end-entity certificate, given the Node ID configured for the peer by other means, if any (Section 4.2).
///
/// A certificate with NODE-IDs authenticates every node ID they carry; if a Node ID is also configured, it must be one of them. A certificate without a NODE-ID authenticates only a configured Node ID.
///
/// # Errors
///
/// [`Error::Absent`] if neither the certificate nor configuration names a Node ID, [`Error::Mismatch`] if the configured Node ID is not among the certificate's, and [`Error::Certificate`] if the certificate cannot be parsed.
pub fn authenticate(der: &[u8], configured: Option<&NodeId>) -> Result<Vec<NodeId>> {
    let presented = node_ids(der)?;
    match configured {
        None if presented.is_empty() => Err(Error::Absent),
        None => Ok(presented),
        Some(configured) if presented.is_empty() => Ok(vec![configured.clone()]),
        Some(configured) if presented.contains(configured) => Ok(presented),
        Some(configured) => Err(Error::Mismatch {
            configured: configured.clone(),
            presented,
        }),
    }
}

/// The node ID a BundleEID otherName value carries: the `[0] EXPLICIT` value holding an IA5String URI. `None` if the value is malformed, or its URI is not the node ID of a remote node.
fn bundle_eid_node_id(value: &[u8]) -> Option<NodeId> {
    let (rest, uri) = TaggedExplicit::<Ia5String, Asn1Error, 0>::from_der(value).ok()?;
    if !rest.is_empty() {
        return None;
    }
    match uri.as_ref().as_ref().parse() {
        Ok(NodeId::LocalNode) | Err(_) => None,
        Ok(node_id) => Some(node_id),
    }
}

#[cfg(test)]
pub mod tests {
    use rcgen::{CertificateParams, CustomExtension, KeyPair};

    use super::*;

    /// The value encodings a test certificate's BundleEID entry can use.
    #[derive(Debug, Clone, Copy)]
    pub enum Encoding {
        /// The IA5String RFC 9174 Section 4.4.2.1 specifies.
        Ia5String,
        /// A UTF8String, which is not a valid BundleEID value.
        Utf8String,
    }

    // A DER TLV (X.690 Section 8.1.3: short-form length below 128 octets,
    // long form above).
    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let len = content.len();
        let mut der = vec![tag];
        if len < 0x80 {
            der.push(len as u8);
        } else {
            let octets = len.to_be_bytes();
            let skip = octets.iter().take_while(|octet| **octet == 0).count();
            der.push(0x80 | (octets.len() - skip) as u8);
            der.extend_from_slice(&octets[skip..]);
        }
        der.extend_from_slice(content);
        der
    }

    /// A subjectAltName extension carrying one otherName entry per `(type-id content octets, value)` pair, for certificates rcgen cannot express: its own otherName support writes only UTF8String values.
    pub fn subject_alt_name(entries: &[(&[u8], &str, Encoding)]) -> CustomExtension {
        const OID: u8 = 0x06;
        const IA5_STRING: u8 = 0x16;
        const UTF8_STRING: u8 = 0x0c;
        const CONTEXT_0: u8 = 0xa0;
        const SEQUENCE: u8 = 0x30;

        let names: Vec<u8> = entries
            .iter()
            .flat_map(|(type_id, uri, encoding)| {
                let tag = match encoding {
                    Encoding::Ia5String => IA5_STRING,
                    Encoding::Utf8String => UTF8_STRING,
                };
                let value = tlv(CONTEXT_0, &tlv(tag, uri.as_bytes()));
                tlv(CONTEXT_0, &[tlv(OID, type_id), value].concat())
            })
            .collect();
        // id-ce-subjectAltName, 2.5.29.17.
        CustomExtension::from_oid_content(&[2, 5, 29, 17], tlv(SEQUENCE, &names))
    }

    /// A subjectAltName extension carrying one IA5String BundleEID entry per URI.
    pub fn bundle_eids(uris: &[&str]) -> CustomExtension {
        let entries: Vec<_> = uris
            .iter()
            .map(|uri| (ID_ON_BUNDLE_EID, *uri, Encoding::Ia5String))
            .collect();
        subject_alt_name(&entries)
    }

    // A self-signed certificate with `san`, or no subjectAltName.
    fn cert(san: Option<CustomExtension>) -> Vec<u8> {
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.custom_extensions.extend(san);
        let key = KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().der().to_vec()
    }

    fn node_id(s: &str) -> NodeId {
        s.parse().unwrap()
    }

    #[test]
    fn every_node_id_entry_is_read_in_order() {
        let der = cert(Some(bundle_eids(&[
            "ipn:977000.1.0",
            "dtn://node.example/",
        ])));
        assert_eq!(
            node_ids(&der).unwrap(),
            [node_id("ipn:977000.1.0"), node_id("dtn://node.example/")]
        );
    }

    #[test]
    fn a_certificate_without_a_subject_alt_name_has_no_node_ids() {
        assert_eq!(node_ids(&cert(None)).unwrap(), []);
    }

    #[test]
    fn an_endpoint_id_that_is_not_a_node_id_is_ignored() {
        let der = cert(Some(bundle_eids(&[
            "ipn:977000.1.5",
            "dtn://node.example/service",
            "dtn:none",
            "ipn:!.0",
            "not a uri",
            "ipn:977000.2.0",
        ])));
        assert_eq!(node_ids(&der).unwrap(), [node_id("ipn:977000.2.0")]);
    }

    #[test]
    fn a_bundle_eid_not_encoded_as_an_ia5_string_is_ignored() {
        let der = cert(Some(subject_alt_name(&[
            (ID_ON_BUNDLE_EID, "ipn:977000.1.0", Encoding::Utf8String),
            (ID_ON_BUNDLE_EID, "ipn:977000.2.0", Encoding::Ia5String),
        ])));
        assert_eq!(node_ids(&der).unwrap(), [node_id("ipn:977000.2.0")]);
    }

    #[test]
    fn another_other_name_form_is_ignored() {
        // id-on-SmtpUTF8Mailbox (RFC 8398), 1.3.6.1.5.5.7.8.9.
        let smtp_utf8_mailbox: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x08, 0x09];
        let der = cert(Some(subject_alt_name(&[
            (smtp_utf8_mailbox, "ipn:977000.1.0", Encoding::Ia5String),
            (ID_ON_BUNDLE_EID, "ipn:977000.2.0", Encoding::Ia5String),
        ])));
        assert_eq!(node_ids(&der).unwrap(), [node_id("ipn:977000.2.0")]);
    }

    #[test]
    fn data_that_is_not_a_certificate_is_refused() {
        assert!(matches!(
            node_ids(&[0x30, 0x00]),
            Err(Error::Certificate(_))
        ));
    }

    #[test]
    fn without_configuration_the_certificate_must_name_the_peer() {
        let der = cert(Some(bundle_eids(&["ipn:977000.1.0", "ipn:977000.2.0"])));
        assert_eq!(
            authenticate(&der, None).unwrap(),
            [node_id("ipn:977000.1.0"), node_id("ipn:977000.2.0")]
        );
        assert!(matches!(
            authenticate(&cert(None), None),
            Err(Error::Absent)
        ));
    }

    #[test]
    fn a_configured_node_id_stands_in_for_an_absent_node_id() {
        let configured = node_id("ipn:977000.1.0");
        assert_eq!(
            authenticate(&cert(None), Some(&configured)).unwrap(),
            [configured]
        );
    }

    #[test]
    fn a_configured_node_id_must_be_among_the_certificates() {
        let der = cert(Some(bundle_eids(&["ipn:977000.1.0", "ipn:977000.2.0"])));
        assert_eq!(
            authenticate(&der, Some(&node_id("ipn:977000.2.0"))).unwrap(),
            [node_id("ipn:977000.1.0"), node_id("ipn:977000.2.0")]
        );
        match authenticate(&der, Some(&node_id("ipn:977000.3.0"))) {
            Err(Error::Mismatch {
                configured,
                presented,
            }) => {
                assert_eq!(configured, node_id("ipn:977000.3.0"));
                assert_eq!(
                    presented,
                    [node_id("ipn:977000.1.0"), node_id("ipn:977000.2.0")]
                );
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    // Node IDs compare after scheme-based normalization (RFC 9174 Section
    // 4.4.1): the two-element ipn form names the same node as the
    // three-element form with the default allocator.
    #[test]
    fn node_ids_match_after_normalization() {
        let der = cert(Some(bundle_eids(&["ipn:0.5.0"])));
        assert_eq!(
            authenticate(&der, Some(&node_id("ipn:5.0"))).unwrap(),
            [node_id("ipn:5.0")]
        );
    }
}
