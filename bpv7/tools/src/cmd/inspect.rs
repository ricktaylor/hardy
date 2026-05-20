use super::*;
use hardy_bpv7::*;

#[derive(Parser, Debug)]
#[command(
    about = "Inspect and display bundle information",
    long_about = "Inspect and display bundle information.\n\n\
        Displays bundle structure, blocks, flags, and security information in various \
        formats. If keys are provided, encrypted blocks will be decrypted for inspection.\n\n\
        Output formats:\n\
        - markdown: Human-readable format (default)\n\
        - json: Machine-readable JSON\n\
        - json-pretty: Pretty-printed JSON"
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// Output format
    #[arg(
        long,
        default_value = "markdown",
        value_name = "FORMAT",
        help = "Output format: markdown (human-readable), json, json-pretty"
    )]
    format: OutputFormat,

    /// Path to the location to write the output to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file to inspect, '-' to use stdin.
    input: io::Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    /// Human-readable markdown format
    Markdown,
    /// Machine-readable JSON format
    Json,
    /// Pretty-printed JSON format
    #[value(name = "json-pretty")]
    JsonPretty,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: bpsec::key::KeySet = self.key_args.try_into()?;
        let bundle_data = self.input.read_all()?;

        // Structural parse + keyed BPSec validation in one pass.
        let parse::Parsed {
            data: bundle_data,
            bundle: raw,
            bcbs: bcb_ops,
            bibs: bib_ops,
        } = parse_with_keys(bundle_data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

        // Section A diagnostics — `report_unsupported` is the OR of the
        // three classify_* outputs. (Errors propagate from parse_with_keys;
        // here we re-run the classify pass to read the flag.)
        let report_unsupported = checks::classify_unrecognised_blocks(&raw.blocks, &[])
            .map(|c| c.report_unsupported)
            .unwrap_or(false)
            || checks::classify_unsupported_bcbs(&raw.blocks, &bcb_ops)
                .map(|c| c.report_unsupported)
                .unwrap_or(false)
            || checks::classify_unsupported_bibs(&raw.blocks, &bib_ops)
                .map(|c| c.report_unsupported)
                .unwrap_or(false);

        // Decode the known extension blocks (PreviousNode / BundleAge /
        // HopCount) once into typed fields. BCB-protected bodies stay
        // opaque (decoded from plaintext only). Both output paths consume
        // this: JSON serializes the fields, markdown reads them in its
        // block walk (see `dump_block`).
        let ext = extension_fields(&bundle_data, &raw.blocks)
            .map_err(|e| anyhow::anyhow!("Failed to decode extension fields: {e}"))?;

        match self.format {
            OutputFormat::Markdown => dump_markdown(
                &raw,
                &ext,
                report_unsupported,
                &bundle_data,
                self.output,
                &BlockSecurity {
                    keys: key_store,
                    bib_ops,
                    bcb_ops,
                },
            ),
            OutputFormat::Json | OutputFormat::JsonPretty => dump_json(
                &raw,
                &ext,
                self.format == OutputFormat::JsonPretty,
                self.output,
            ),
        }
    }
}

/// The BPSec material `dump_block` needs to verify and decrypt blocks:
/// the key set plus the parsed BIB/BCB operation sets. Bundled so the
/// markdown path threads one value instead of three.
struct BlockSecurity {
    keys: bpsec::key::KeySet,
    bib_ops: std::collections::HashMap<u64, bpsec::bib::OperationSet>,
    bcb_ops: std::collections::HashMap<u64, bpsec::bcb::OperationSet>,
}

/// The typed values decoded from a bundle's known extension blocks, plus
/// whether any was non-canonically encoded. Inspect-local — `validate`
/// and `full_rewrite` have their own focused variants.
#[derive(Default)]
struct ExtFields {
    previous_node: Option<eid::Eid>,
    age: Option<core::time::Duration>,
    hop_count: Option<hop_info::HopInfo>,
    non_canonical: bool,
}

/// Decode PreviousNode / BundleAge / HopCount bodies from their plaintext
/// wire bytes. Encrypted bodies (`b.bcb.is_some()`) are opaque and left
/// at their `None` default. Errors on a malformed body.
fn extension_fields(
    data: &[u8],
    blocks: &std::collections::HashMap<u64, block::Block>,
) -> Result<ExtFields, hardy_bpv7::Error> {
    let mut out = ExtFields::default();
    for b in blocks.values() {
        if b.bcb.is_some() {
            continue;
        }
        // `data` is the full in-memory bundle from `parse_with_keys`.
        let Some(body) = b.payload(data) else {
            continue;
        };
        match b.block_type {
            block::Type::PreviousNode => {
                let (v, shortest) = parse_exact::<(eid::Eid, bool)>(body, "Previous Node Block")?;
                out.non_canonical |= !shortest;
                out.previous_node = Some(v);
            }
            block::Type::BundleAge => {
                let v = parse_exact::<bundle_age::BundleAge>(body, "Bundle Age Block")?;
                out.age = Some(v.into());
            }
            block::Type::HopCount => {
                let (v, shortest) =
                    parse_exact::<(hop_info::HopInfo, bool)>(body, "Hop Count Block")?;
                out.non_canonical |= !shortest;
                out.hop_count = Some(v);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The `bundle inspect --format json` serialisation shape, built
/// by-reference from a `Bundle` plus the decoded [`ExtFields`]. Field
/// order and `#[serde(...)]` attributes define the tool's stable JSON
/// output (covered by the shell tests).
#[derive(serde::Serialize)]
struct JsonBundle<'a> {
    #[serde(flatten)]
    id: &'a bundle::Id,
    flags: &'a bundle::Flags,
    crc_type: crc::CrcType,
    destination: &'a eid::Eid,
    report_to: &'a eid::Eid,
    lifetime: core::time::Duration,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_node: Option<&'a eid::Eid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    age: Option<core::time::Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hop_count: Option<&'a hop_info::HopInfo>,
    blocks: &'a std::collections::HashMap<u64, block::Block>,
}

fn dump_json(
    raw: &Bundle,
    ext: &ExtFields,
    pretty: bool,
    output: io::Output,
) -> anyhow::Result<()> {
    if ext.non_canonical {
        eprintln!("Warning: Non-canonical, but semantically valid bundle");
    }

    let bundle = JsonBundle {
        id: &raw.primary.id,
        flags: &raw.primary.flags,
        crc_type: raw.primary.crc_type,
        destination: &raw.primary.destination,
        report_to: &raw.primary.report_to,
        lifetime: raw.primary.lifetime,
        previous_node: ext.previous_node.as_ref(),
        age: ext.age,
        hop_count: ext.hop_count.as_ref(),
        blocks: &raw.blocks,
    };

    let mut json = if pretty {
        serde_json::to_string_pretty(&bundle)
    } else {
        serde_json::to_string(&bundle)
    }
    .map_err(|e| anyhow::anyhow!("Failed to serialize bundle: {e}"))?;
    json.push('\n');

    output.write_all(json.as_bytes())
}

fn dump_markdown(
    raw: &Bundle,
    ext: &ExtFields,
    report_unsupported: bool,
    data: &[u8],
    output: io::Output,
    security: &BlockSecurity,
) -> anyhow::Result<()> {
    let primary = &raw.primary;
    if let Some(fragment_info) = &primary.id.fragment_info {
        output.write_str(format!(
            "# BPv7 ADU Fragment {} of {}\n\n",
            fragment_info.offset, fragment_info.total_adu_length
        ))?;
    } else {
        output.write_str("# BPv7 Bundle\n\n")?;
    }

    output.append_str(format!("Source: {}\n\n", primary.id.source))?;
    output.append_str(format!("Destination: {}\n\n", primary.destination))?;
    output.append_str(format!("Created: {}\n\n", primary.id.timestamp))?;
    output.append_str(format!(
        "Lifetime: {}\n\n",
        humantime::format_duration(primary.lifetime)
    ))?;

    let mut notes: Vec<&'static str> = Vec::new();

    if ext.non_canonical {
        notes.push("The bundle is not in canonical form\n");
    }

    if report_unsupported {
        notes.push("The bundle contains unsupported blocks\n");
    }

    dump_crc(primary.crc_type, &output)?;

    if primary.flags == bundle::Flags::default() {
        output.append_str("Bundle Flags: None\n\n")?;
    } else {
        output.append_str("Bundle Flags:\n\n")?;

        if primary.flags.is_fragment {
            output.append_str("* Is a fragment\n")?;

            if primary.flags.is_admin_record {
                output.append_str("* ADU is an Administrative Record\n")?;
            }

            if primary.flags.do_not_fragment {
                output.append_str("* Do not fragment\n")?;
            }

            if primary.flags.app_ack_requested {
                output.append_str("* Application acknowledgement requested\n")?;
            }

            if primary.flags.report_status_time {
                output.append_str("* Include status time with reports\n")?;

                if !primary.flags.receipt_report_requested
                    || !primary.flags.forward_report_requested
                    || !primary.flags.delivery_report_requested
                    || !primary.flags.delete_report_requested
                {
                    notes.push("Bundle flags request status time to be included with status reports, but no reports are requested.");
                }
            }

            if primary.flags.receipt_report_requested {
                output.append_str("* Reception report requested\n")?;
            }

            if primary.flags.forward_report_requested {
                output.append_str("* Forwarding report requested\n")?;
            }

            if primary.flags.delivery_report_requested {
                output.append_str("* Delivery report requested\n")?;
            }

            if primary.flags.delete_report_requested {
                output.append_str("* Deletion report requested\n")?;
            }

            if let Some(u) = primary.flags.unrecognised {
                output.append_str(format!("* Unrecognised: {u:#x}\n",))?;
            }

            output.append_str("\n")?;

            if (primary.flags.receipt_report_requested
                || primary.flags.forward_report_requested
                || primary.flags.delivery_report_requested
                || primary.flags.delete_report_requested)
                && primary.report_to.is_null()
            {
                notes.push("Null endpoint EID specified for 'Report To', but status reports are requested.");
            }
        }
    }

    output.append_str(format!("Report-To: {}\n\n", primary.report_to))?;

    if !notes.is_empty() {
        output.append_str("**Notes:**\n\n")?;
        for (idx, note) in notes.into_iter().enumerate() {
            output.append_str(format!("{idx}. {note}\n"))?;
        }
        output.append_str("\n")?;
    }

    let blocks = raw.blocks.keys().cloned().collect::<Vec<_>>();
    let mut blocks = blocks
        .into_iter()
        .map(|n| (n, raw.blocks.get(&n).unwrap()))
        .collect::<Vec<_>>();

    blocks.sort_by_key(|a| a.1.extent.start);

    for (block_number, block) in blocks {
        if block_number != 0 {
            dump_block(
                &raw.blocks,
                block_number,
                block,
                data,
                &output,
                security,
                ext,
            )?;
        }
    }

    Ok(())
}

fn dump_crc(crc: crc::CrcType, output: &io::Output) -> anyhow::Result<()> {
    output.append_str("CRC: ")?;
    match crc {
        crc::CrcType::None => output.append_str("None"),
        crc::CrcType::Unrecognised(u) => output.append_str(format!("Unrecognised ({u})")),
        crc::CrcType::CRC16_X25 => output.append_str("16-bit (type 1)"),
        crc::CrcType::CRC32_CASTAGNOLI => output.append_str("32-bit (type 2)"),
    }?;
    output.append_str("\n\n")
}

fn dump_block(
    blocks: &std::collections::HashMap<u64, block::Block>,
    block_number: u64,
    block: &block::Block,
    data: &[u8],
    output: &io::Output,
    security: &BlockSecurity,
    ext: &ExtFields,
) -> anyhow::Result<()> {
    output.append_str(format!("## Block {block_number}: "))?;
    match &block.block_type {
        block::Type::Primary => unreachable!("Primary block handled separately"),
        block::Type::Payload => output.append_str("Payload\n\n"),
        block::Type::PreviousNode => output.append_str("Previous Node\n\n"),
        block::Type::BundleAge => output.append_str("Bundle Age\n\n"),
        block::Type::HopCount => output.append_str("Hop Count\n\n"),
        block::Type::BlockIntegrity => output.append_str("Block Integrity\n\n"),
        block::Type::BlockSecurity => output.append_str("Block Security\n\n"),
        block::Type::Unrecognised(u) => output.append_str(format!("Unrecognised Type {u}\n\n")),
    }?;

    dump_crc(block.crc_type, output)?;

    if block.flags == block::Flags::default() {
        output.append_str("Block Flags: None\n\n")?;
    } else {
        output.append_str("Block Flags:\n\n")?;

        if block.flags.must_replicate {
            output.append_str("* Must replicate\n")?;
        }

        if block.flags.report_on_failure {
            output.append_str("* Report on failure\n")?;
        }

        if block.flags.delete_block_on_failure {
            output.append_str("* Delete block on failure\n")?;
        }

        if block.flags.delete_bundle_on_failure {
            output.append_str("* Delete bundle on failure\n")?;
        }

        if let Some(u) = block.flags.unrecognised {
            output.append_str(format!("* Unrecognised: {u:#x}\n"))?;
        }

        output.append_str("\n")?;
    }

    if let hardy_bpv7::block::BibCoverage::Some(bib) = block.bib {
        output.append_str(format!("Signed by Integrity Block {bib}: "))?;

        match verify_block(
            block_number,
            blocks,
            data,
            &security.bib_ops,
            &security.keys,
        ) {
            Err(e) => output.append_str(format!("Error {e}\n\n"))?,
            Ok(_) => output.append_str("✔\n\n")?,
        }
    } else if matches!(block.bib, hardy_bpv7::block::BibCoverage::Maybe) {
        output.append_str("Signed by Integrity Block: Unknown (encrypted BIB)\n\n")?;
    }

    let payload = if let Some(bcb) = block.bcb {
        output.append_str(format!("Encrypted by Security Block {bcb}: "))?;

        match bpsec::block_data(
            block_number,
            blocks,
            data,
            &security.bcb_ops,
            &security.keys,
        ) {
            Err(e) => {
                output.append_str(format!("Error {e}\n\n"))?;
                None
            }
            Ok(p) => {
                output.append_str("✔\n\n")?;
                Some(p)
            }
        }
    } else {
        Some(
            bpsec::block_data(
                block_number,
                blocks,
                data,
                &security.bcb_ops,
                &security.keys,
            )
            .map_err(|e| anyhow::anyhow!("Failed to get block data: {e}"))?,
        )
    };

    if let Some(payload) = payload {
        // Known extension blocks read their already-decoded value from
        // `ext` (the single unpack shared with the JSON view); the
        // security and opaque blocks decode their payload here, mirroring
        // `dump_bib` / `dump_bcb`. A `None` field with a present payload
        // is a BCB-decrypted known block (`ext` only unpacks plaintext) —
        // fall back to the raw CBOR dump.
        match block.block_type {
            block::Type::Primary => unreachable!("Primary block handled separately"),
            block::Type::PreviousNode => match &ext.previous_node {
                Some(eid) => output.append_str(format!("Previous Node: {eid}\n\n")),
                None => dump_unknown(payload.as_ref(), output),
            },
            block::Type::BundleAge => match ext.age {
                Some(age) => output.append_str(format!(
                    "Bundle Age: {}\n\n",
                    humantime::format_duration(age)
                )),
                None => dump_unknown(payload.as_ref(), output),
            },
            block::Type::HopCount => match &ext.hop_count {
                Some(hop_count) => output.append_str(format!(
                    "Hop Count: {} of {}\n\n",
                    hop_count.count, hop_count.limit
                )),
                None => dump_unknown(payload.as_ref(), output),
            },
            block::Type::BlockIntegrity => dump_bib(payload.as_ref(), output),
            block::Type::BlockSecurity => dump_bcb(payload.as_ref(), output),
            block::Type::Payload | block::Type::Unrecognised(_) => {
                dump_unknown(payload.as_ref(), output)
            }
        }
    } else {
        // Length of the block-specific data byte string. `block.data` is
        // already relative to the block extent, so `end - start` gives the
        // body length without going through `payload_range`.
        output.append_str(format!(
            "### Block Specific Data\n\n{} bytes of encrypted data\n\n",
            block.data.end - block.data.start
        ))
    }
}

fn dump_unknown(mut data: &[u8], output: &io::Output) -> anyhow::Result<()> {
    output.append_str("### Block Specific Data\n\n")?;

    if data.is_empty() {
        return output.append_str("None\n");
    }

    if let Ok(s) = str::from_utf8(data)
        && !s.contains(|c: char| c.is_control())
    {
        return output.append_str(format!("`{s}`\n"));
    }

    let mut results = Vec::new();
    while let Ok((s, len)) = hardy_cbor::decode::parse_value(data, |mut v, _, _| {
        let s = format!("{v:?}");
        v.skip(16)?;
        Ok::<_, hardy_cbor::decode::Error>(s)
    }) {
        results.push(s);

        if len <= data.len() {
            data = &data[len..];
        } else {
            break;
        }
    }

    if !results.is_empty() {
        output.append_str("Probably CBOR\n")?;
        for s in results {
            output.append_str(format!("\n`{s}`\n"))?;
        }

        if data.is_empty() {
            return Ok(());
        }

        output.append_str("\nFollowed by ")?;
    }

    output.append_str(format!(
        "{} bytes of data in an unrecognized format\n",
        data.len()
    ))
}

fn dump_bcb(data: &[u8], output: &io::Output) -> anyhow::Result<()> {
    let ops = hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(data)?;
    output.append_str(format!(
        "### BCB Data\n\nSecurity Source: {}\n\n",
        ops.source()
    ))?;

    match ops.operations().values().next().unwrap() {
        bpsec::bcb::Operation::AES_GCM(op) => {
            output.append_str(format!(
                "Context: BCB-AES-GCM {:?}\n\n",
                op.parameters.variant
            ))?;
            output.append_str(format!(
                "IV: ({} bits) {}\n\n",
                op.parameters.iv.len() * 8,
                dump_bytes(&op.parameters.iv),
            ))?;
            if let Some(key) = &op.parameters.key {
                output.append_str(format!("Wrapped Key: {}\n\n", dump_bytes(key),))?;
            }

            if op.parameters.flags == bpsec::rfc9173::ScopeFlags::NONE {
                output.append_str("Scope Flags: None\n\n")?;
            } else {
                output.append_str("Scope Flags:\n\n")?;

                if op.parameters.flags.include_primary_block {
                    output.append_str("* Include primary block\n")?;
                }

                if op.parameters.flags.include_target_header {
                    output.append_str("* Include target header\n")?;
                }

                if op.parameters.flags.include_security_header {
                    output.append_str("* Include security header\n")?;
                }

                if let Some(u) = op.parameters.flags.unrecognised {
                    output.append_str(format!("* Unrecognised: {u:#x}\n"))?;
                }

                output.append_str("\n")?;
            }
        }
        bpsec::bcb::Operation::Unrecognised(_u, op) => {
            output.append_str("Context: Unrecognised Type {u}\n\n")?;
            for (p, v) in op.parameters.iter() {
                output.append_str(format!("Parameter {p}: {}\n\n", dump_bytes(v)))?;
            }
        }
    }

    for (target, op) in ops.operations() {
        output.append_str(format!("#### Target Block {target}\n\n"))?;

        match op {
            bpsec::bcb::Operation::AES_GCM(op) => {
                if let Some(tag) = &op.results.0 {
                    output.append_str(format!("Authentication Tag: {}\n\n", dump_bytes(tag)))?;
                } else {
                    output.append_str("Authentication Tag: None\n\n")?;
                }
            }
            bpsec::bcb::Operation::Unrecognised(_u, op) => {
                for (r, v) in op.results.iter() {
                    output.append_str(format!("Result {}: {}\n\n", r, dump_bytes(v)))?;
                }
            }
        }
    }

    Ok(())
}

fn dump_bib(data: &[u8], output: &io::Output) -> anyhow::Result<()> {
    let ops = hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(data)?;
    output.append_str(format!(
        "### BIB Data\n\nSecurity Source: {}\n\n",
        ops.source()
    ))?;

    match ops.operations().values().next().unwrap() {
        bpsec::bib::Operation::HMAC_SHA2(op) => {
            output.append_str(format!(
                "Context: BIB-HMAC-SHA2 {:?}\n\n",
                op.parameters.variant
            ))?;
            if let Some(key) = &op.parameters.key {
                output.append_str(format!("Wrapped Key: {}\n\n", dump_bytes(key),))?;
            }

            if op.parameters.flags == bpsec::rfc9173::ScopeFlags::NONE {
                output.append_str("Scope Flags: None\n\n")?;
            } else {
                output.append_str("Scope Flags:\n\n")?;

                if op.parameters.flags.include_primary_block {
                    output.append_str("* Include primary block\n")?;
                }

                if op.parameters.flags.include_target_header {
                    output.append_str("* Include target header\n")?;
                }

                if op.parameters.flags.include_security_header {
                    output.append_str("* Include security header\n")?;
                }

                if let Some(u) = op.parameters.flags.unrecognised {
                    output.append_str(format!("* Unrecognised: {u:#x}\n"))?;
                }

                output.append_str("\n")?;
            }
        }
        bpsec::bib::Operation::Unrecognised(_u, op) => {
            output.append_str("Context: Unrecognised Type {u}\n\n")?;
            for (p, v) in op.parameters.iter() {
                output.append_str(format!("Parameter {p}: {}\n\n", dump_bytes(v)))?;
            }
        }
    }

    for (target, op) in ops.operations() {
        output.append_str(format!("#### Target Block: {target}\n\n"))?;

        match op {
            bpsec::bib::Operation::HMAC_SHA2(op) => {
                output.append_str(format!("HMAC: {}\n\n", dump_bytes(&op.results.0)))?;
            }
            bpsec::bib::Operation::Unrecognised(_u, op) => {
                for (r, v) in op.results.iter() {
                    output.append_str(format!("Result {r}: {}\n\n", dump_bytes(v)))?;
                }
            }
        }
    }

    Ok(())
}

fn dump_bytes(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}
