use super::*;
use hardy_bpv7::{builder::Builder, hop_info::HopInfo};
use hardy_cbor::{decode, encode};

/// CBOR payload format per PING_SPEC.md Appendix C.
///
/// Structure: `[sequence, options_map]`
///
/// Options map keys:
/// - 0: Padding (bstr) - for MTU testing
#[repr(u64)]
enum OptionKey {
    Padding = 0,
}

/// Parsed ping payload.
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
}

impl encode::ToCbor for Payload {
    type Result = ();

    fn to_cbor(&self, encoder: &mut encode::Encoder) -> Self::Result {
        // Count options for definite-length map
        let opt_count = if self.padding_len > 0 { 1 } else { 0 };

        // Emit 2-element array: [sequence, options_map]
        encoder.emit_array(Some(2), |a| {
            a.emit(&self.seqno);

            a.emit_map(Some(opt_count), |m| {
                // Padding (key 0)
                if self.padding_len > 0 {
                    m.emit(&(OptionKey::Padding as u64));
                    // Zero-filled padding
                    m.emit(&encode::Bytes(&vec![0u8; self.padding_len]));
                }
            });
        });
    }
}

impl decode::FromCbor for Payload {
    type Error = decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        decode::parse_array(data, |arr, _shortest, _tags| {
            // Parse sequence number
            let seqno: u32 = arr.parse()?;

            // Parse options map
            let mut padding_len = 0;

            arr.parse_map(|map, _shortest, _tags| {
                while !map.at_end()? {
                    let key: u64 = map.parse()?;
                    match key {
                        k if k == OptionKey::Padding as u64 => {
                            // Parse padding bytes, just record length
                            map.parse_value(|value, _shortest, _tags| {
                                if let decode::Value::Bytes(bytes) = value {
                                    padding_len = bytes.len();
                                    Ok(())
                                } else {
                                    Err(decode::Error::IncorrectType(
                                        "bytes".into(),
                                        value.type_name(false),
                                    ))
                                }
                            })?;
                        }
                        _ => {
                            // Skip unknown options
                            map.skip_value(16)?;
                        }
                    }
                }
                Ok::<_, decode::Error>(())
            })?;

            Ok((Payload { seqno, padding_len }, true))
        })
        .map(|((payload, shortest), len)| (payload, shortest, len))
    }
}

/// Build a bundle with the payload, optionally targeting a specific total bundle size.
///
/// If `args.size` is specified, uses binary search to find the exact padding needed
/// to achieve the target bundle size. This accounts for all overhead including:
/// - CBOR length field encoding (variable 1/2/4/8 bytes)
/// - Bundle primary block
/// - Extension blocks (HopCount)
fn build_bundle_with_padding(
    args: &Command,
    seq_no: u32,
    padding: usize,
    creation: time::OffsetDateTime,
) -> anyhow::Result<Box<[u8]>> {
    let payload = Payload::new(seq_no).with_padding(padding);
    let payload_bytes = encode::emit(&payload).0;

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

    Ok(builder
        .with_payload(payload_bytes.into())
        .build(
            creation
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to convert creation time"))?,
        )?
        .1)
}

pub fn build_payload(
    args: &Command,
    seq_no: u32,
) -> anyhow::Result<(Box<[u8]>, time::OffsetDateTime)> {
    let creation = time::OffsetDateTime::now_utc();

    (args.lifetime().as_millis() <= u64::MAX as u128)
        .then_some(())
        .ok_or(anyhow::anyhow!(
            "Lifetime too long: {}!",
            humantime::format_duration(args.lifetime())
        ))?;

    let bundle = if let Some(target_size) = args.size {
        // Binary search for exact bundle size
        // First check if minimum bundle size (no padding) already exceeds target
        let min_bundle = build_bundle_with_padding(args, seq_no, 0, creation)?;
        if min_bundle.len() > target_size {
            return Err(anyhow::anyhow!(
                "Minimum bundle size ({} bytes) exceeds target size ({} bytes)",
                min_bundle.len(),
                target_size
            ));
        }
        if min_bundle.len() == target_size {
            min_bundle
        } else {
            // Binary search: padding in [0, target_size]
            // More padding = larger bundle (monotonic)
            let mut low = 0usize;
            let mut high = target_size;

            loop {
                let mid = (low + high) / 2;
                let bundle = build_bundle_with_padding(args, seq_no, mid, creation)?;

                match bundle.len().cmp(&target_size) {
                    std::cmp::Ordering::Equal => break bundle,
                    std::cmp::Ordering::Less => low = mid + 1,
                    std::cmp::Ordering::Greater => high = mid.saturating_sub(1),
                }

                // Convergence check - if we can't make progress, take closest match
                if low > high {
                    // Try both bounds and pick the one that gets us closest
                    let bundle_low = build_bundle_with_padding(args, seq_no, low, creation)?;
                    if bundle_low.len() == target_size {
                        break bundle_low;
                    }
                    // We couldn't hit exact target (shouldn't happen for reasonable sizes)
                    return Err(anyhow::anyhow!(
                        "Cannot achieve exact target size {} bytes (closest: {} bytes with {} padding)",
                        target_size,
                        bundle_low.len(),
                        low
                    ));
                }
            }
        }
    } else {
        // No size target - build without padding
        build_bundle_with_padding(args, seq_no, 0, creation)?
    };

    Ok((bundle, creation))
}
