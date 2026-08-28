use super::*;
use hardy_bpv7::{block, builder::Builder, crc::CrcType, hop_info::HopInfo};
use hardy_cbor::{decode, encode};

// The echo-service draft defines no payload wire format — the echo reflects the
// payload unchanged and never parses it (see draft-taylor-dtn-echo-service). This
// is purely the `bp ping` client's own internal format: a CBOR-encoded sequence
// number so responses can be matched to requests, followed by optional 0xAA
// padding for path-MTU probing. RTT timing and response validation are held in
// local client state (see service.rs), not carried on the wire.
//
// Wire layout: <CBOR unsigned integer: sequence number><0xAA padding bytes>
//
// The padding is raw bytes rather than a CBOR item, so it never affects the
// decoded sequence number; the client's byte-for-byte payload comparison is what
// verifies the padding round-tripped intact.

// Fill byte for path-MTU padding.
const PADDING_BYTE: u8 = 0xAA;

// Parsed ping payload.
pub struct Payload {
    pub seqno: u32,
    pub padding_len: usize,
}

impl Payload {
    pub fn new(seqno: u32) -> Self {
        Self {
            seqno,
            padding_len: 0,
        }
    }

    pub fn with_padding(mut self, padding_len: usize) -> Self {
        self.padding_len = padding_len;
        self
    }

    // Encode as a CBOR sequence number followed by `padding_len` 0xAA bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = encode::emit(&self.seqno).0;
        bytes.resize(bytes.len() + self.padding_len, PADDING_BYTE);
        bytes
    }

    // Parse the leading CBOR sequence number; any trailing bytes are padding.
    pub fn parse(data: &[u8]) -> Result<Self, decode::Error> {
        let (seqno, _shortest, consumed) = <u32 as decode::FromCbor>::from_cbor(data)?;
        Ok(Self {
            seqno,
            padding_len: data.len() - consumed,
        })
    }
}

// Build a bundle with the payload, optionally targeting a specific total bundle size.
//
// If `args.size` is specified, uses binary search to find the exact padding needed
// to achieve the target bundle size. This accounts for all overhead including:
// - CBOR length field encoding (variable 1/2/4/8 bytes)
// - Bundle primary block
// - Extension blocks (HopCount)
fn build_bundle_with_padding(
    args: &Command,
    seq_no: u32,
    padding: usize,
    creation: time::OffsetDateTime,
) -> anyhow::Result<BuiltPing> {
    let payload = Payload::new(seq_no).with_padding(padding);
    let payload_bytes = payload.to_bytes();

    let source = args.source.clone().unwrap();

    // Always request all status reports - receiver decides whether to generate them
    let bundle_flags = hardy_bpv7::bundle::Flags {
        report_status_time: true,
        receipt_report_requested: true,
        forward_report_requested: true,
        delivery_report_requested: true,
        delete_report_requested: true,
        ..Default::default()
    };

    let mut builder = Builder::new(source.clone(), args.destination.clone())
        .with_report_to(source)
        .with_flags(bundle_flags)
        .with_lifetime(args.lifetime());

    // Add HopCount block if TTL specified (like IP TTL)
    if let Some(ttl) = args.ttl {
        builder = builder.with_hop_count(&HopInfo {
            limit: ttl,
            count: 0,
        });
    }

    // Add payload - with optional CRC control for DTNME compatibility
    if args.no_payload_crc {
        // DTNME compatibility mode: Keep CRC on primary block, but no CRC on payload
        // DTNME has a bug where it doesn't validate payload block CRC but rejects
        // bundles when CRC validation fails.
        builder = builder
            .add_extension_block(block::Type::Payload)
            .expect("Failed to add payload block")
            .with_flags(block::Flags {
                delete_bundle_on_failure: true,
                ..Default::default()
            })
            .with_crc_type(CrcType::None)
            .build(payload_bytes.as_slice().into());
    } else {
        // Normal mode: CRC on all blocks (default CRC32-C)
        builder = builder.with_payload(payload_bytes.as_slice().into());
    }

    let bundle = builder
        .build(
            creation
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to convert creation time"))?,
        )?
        .1;

    Ok((bundle, payload_bytes.into()))
}

// (wire bundle bytes, payload bytes). The client retains the payload bytes to
// compare against the reflected response for round-trip integrity.
pub type BuiltPing = (Box<[u8]>, Box<[u8]>);

pub fn build_payload(args: &Command, seq_no: u32) -> anyhow::Result<BuiltPing> {
    let creation = time::OffsetDateTime::now_utc();

    (args.lifetime().as_millis() <= u64::MAX as u128)
        .then_some(())
        .ok_or(anyhow::anyhow!(
            "Lifetime too long: {}!",
            humantime::format_duration(args.lifetime())
        ))?;

    let Some(target_size) = args.size else {
        // No size target - build without padding.
        return build_bundle_with_padding(args, seq_no, 0, creation);
    };

    // Binary search for the padding that hits the exact target bundle size.
    // More padding = larger bundle (monotonic). build_bundle_with_padding returns
    // the payload bytes it built, so there is no need to re-parse the bundle.
    let min = build_bundle_with_padding(args, seq_no, 0, creation)?;
    if min.0.len() > target_size {
        return Err(anyhow::anyhow!(
            "Minimum bundle size ({} bytes) exceeds target size ({} bytes)",
            min.0.len(),
            target_size
        ));
    }
    if min.0.len() == target_size {
        return Ok(min);
    }

    let mut low = 0usize;
    let mut high = target_size;
    loop {
        let mid = (low + high) / 2;
        let built = build_bundle_with_padding(args, seq_no, mid, creation)?;

        match built.0.len().cmp(&target_size) {
            std::cmp::Ordering::Equal => return Ok(built),
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid.saturating_sub(1),
        }

        // Convergence check - if we can't make progress, take the closest match.
        if low > high {
            let built_low = build_bundle_with_padding(args, seq_no, low, creation)?;
            if built_low.0.len() == target_size {
                return Ok(built_low);
            }
            return Err(anyhow::anyhow!(
                "Cannot achieve exact target size {} bytes (closest: {} bytes with {} padding)",
                target_size,
                built_low.0.len(),
                low
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn base_command() -> Command {
        Command::try_parse_from(["bp-ping", "-S", "ipn:1.1", "ipn:2.7"]).unwrap()
    }

    // The wire format is a CBOR sequence number followed by raw 0xAA padding.
    #[test]
    fn test_payload_round_trip() {
        for padding in [0usize, 1, 1000] {
            let bytes = Payload::new(u32::MAX).with_padding(padding).to_bytes();

            // u32::MAX encodes as a 5-byte CBOR unsigned integer.
            assert_eq!(bytes.len(), 5 + padding);
            assert!(bytes[5..].iter().all(|b| *b == PADDING_BYTE));

            let parsed = Payload::parse(&bytes).unwrap();
            assert_eq!(parsed.seqno, u32::MAX);
            assert_eq!(parsed.padding_len, padding);
        }

        assert!(Payload::parse(&[]).is_err());
    }

    // Sweep every target size across a window that includes the CBOR
    // byte-string length-field boundaries (payload 23->24 and 255->256 bytes):
    // each target must yield either a bundle of exactly that size or a clean
    // error for the sizes the length-field jumps make unreachable — never a
    // wrong-sized Ok.
    fn run_size_sweep() {
        let mut args = base_command();

        let base_len = build_payload(&args, 0).unwrap().0.len();

        // Below the minimum: a clean error.
        args.size = Some(base_len - 1);
        assert!(build_payload(&args, 0).is_err());

        let mut unreachable_targets = 0;
        for target in base_len..base_len + 300 {
            args.size = Some(target);
            match build_payload(&args, 0) {
                Ok((bundle, payload)) => {
                    assert_eq!(
                        bundle.len(),
                        target,
                        "build_payload returned a wrong-sized bundle for target {target}"
                    );
                    // The reflected-payload comparison depends on the returned
                    // payload bytes matching what was actually built in.
                    let parsed = Payload::parse(&payload).unwrap();
                    assert_eq!(parsed.seqno, 0);
                }
                Err(_) => unreachable_targets += 1,
            }
        }

        // The payload byte-string length field widens twice in this window,
        // each jump skipping exactly one target size.
        assert!(
            (1..=4).contains(&unreachable_targets),
            "unexpected number of unreachable target sizes: {unreachable_targets}"
        );
    }

    #[test]
    fn test_build_payload_exact_sizes() {
        // Watchdog, not synchronization: the sweep is pure computation and
        // finishes in well under a second. The generous bound only converts a
        // non-terminating binary-search regression into a test failure instead
        // of a hung test run.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            run_size_sweep();
            tx.send(()).ok();
        });
        rx.recv_timeout(std::time::Duration::from_secs(120))
            .expect("build_payload size sweep did not terminate");
    }
}
