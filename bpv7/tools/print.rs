use super::*;
use hardy_bpv7::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// Path to the location to write the output to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file to print, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: bpsec::key::KeySet = self.key_args.try_into()?;

        let bundle = self.input.read_all()?;

        dump_bundle(
            bundle::ParsedBundle::parse(&bundle, &key_store)
                .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?,
            &bundle,
            self.output,
            key_store,
        )
    }
}

fn dump_bundle(
    bundle: bundle::ParsedBundle,
    data: &[u8],
    output: io::Output,
    keys: bpsec::key::KeySet,
) -> anyhow::Result<()> {
    if let Some(fragment_info) = &bundle.bundle.id.fragment_info {
        output.write_str(format!(
            "# BPv7 ADU Fragment {} of {}\n\n",
            fragment_info.offset, fragment_info.total_adu_length
        ))?;
    } else {
        output.write_str("# BPv7 Bundle\n\n")?;
    }

    output.append_str(format!("Source: {}\n", bundle.bundle.id.source))?;
    output.append_str(format!("Destination: {}\n", bundle.bundle.destination))?;
    output.append_str(format!("Created: {}\n", bundle.bundle.id.timestamp))?;
    output.append_str(format!(
        "Lifetime: {}\n",
        humantime::format_duration(bundle.bundle.lifetime)
    ))?;

    let mut notes = Vec::new();

    if bundle.non_canonical {
        notes.push("The bundle is not in canonical form\n".to_string());
    }

    if bundle.report_unsupported {
        notes.push("The bundle contains unsupported blocks\n".to_string());
    }

    dump_crc(bundle.bundle.crc_type, &output)?;

    if bundle.bundle.flags == bundle::Flags::default() {
        output.append_str("Bundle Flags: None\n")?;
    } else {
        output.append_str("Bundle Flags:\n")?;

        if bundle.bundle.flags.is_fragment {
            output.append_str("  * Is a fragment\n")?;

            if bundle.bundle.flags.is_admin_record {
                output.append_str("  * ADU is an Administrative Record\n")?;
            }

            if bundle.bundle.flags.do_not_fragment {
                output.append_str("  * Do not fragment\n")?;
            }

            if bundle.bundle.flags.app_ack_requested {
                output.append_str("  * Application acknowledgement requested\n")?;
            }

            if bundle.bundle.flags.report_status_time {
                output.append_str("  * Include status time with reports\n")?;

                if !bundle.bundle.flags.receipt_report_requested
                    || !bundle.bundle.flags.forward_report_requested
                    || !bundle.bundle.flags.delivery_report_requested
                    || !bundle.bundle.flags.delete_report_requested
                {
                    notes.push("Bundle flags request status time to be included with status reports, but no reports are requested.".to_string());
                }
            }

            if bundle.bundle.flags.receipt_report_requested {
                output.append_str("  * Reception report requested\n")?;
            }

            if bundle.bundle.flags.forward_report_requested {
                output.append_str("  * Forwarding report requested\n")?;
            }

            if bundle.bundle.flags.delivery_report_requested {
                output.append_str("  * Delivery report requested\n")?;
            }

            if bundle.bundle.flags.delete_report_requested {
                output.append_str("  * Deletion report requested\n")?;
            }

            if let Some(u) = bundle.bundle.flags.unrecognised {
                output.append_str(format!("  * Unrecognised: {u:#x}\n",))?;
            }

            output.append_str("\n")?;

            if (bundle.bundle.flags.receipt_report_requested
                || bundle.bundle.flags.forward_report_requested
                || bundle.bundle.flags.delivery_report_requested
                || bundle.bundle.flags.delete_report_requested)
                && bundle.bundle.report_to.is_null()
            {
                notes.push("Null endpoint EID specified for 'Report To', but status reports are requested.".to_string());
            }
        }
    }

    output.append_str(format!("Report-To: {}\n", bundle.bundle.report_to))?;

    if !notes.is_empty() {
        output.append_str("\n**Notes:**\n")?;
        for (idx, note) in notes.into_iter().enumerate() {
            output.append_str(format!("  {idx}. {note}\n"))?;
        }
    }

    let blocks = bundle.bundle.blocks.keys().cloned().collect::<Vec<_>>();
    let mut blocks = blocks
        .into_iter()
        .map(|n| (n, bundle.bundle.blocks.get(&n).unwrap()))
        .collect::<Vec<_>>();

    blocks.sort_by(|a, b| a.1.extent.start.cmp(&b.1.extent.start));

    for (block_number, block) in blocks {
        if block_number != 0 {
            dump_block(&bundle.bundle, block_number, block, data, &output, &keys)?;
        }
    }

    Ok(())
}

fn dump_crc(crc: crc::CrcType, output: &io::Output) -> anyhow::Result<()> {
    match crc {
        crc::CrcType::None => output.append_str("CRC: None\n"),
        crc::CrcType::Unrecognised(u) => output.append_str(format!("CRC: Unrecognised ({u})\n")),
        crc::CrcType::CRC16_X25 => output.append_str("CRC: 16-bit (type 1)\n"),
        crc::CrcType::CRC32_CASTAGNOLI => output.append_str("CRC: 32-bit (type 2)\n"),
    }
}

fn dump_block(
    bundle: &bundle::Bundle,
    block_number: u64,
    block: &block::Block,
    data: &[u8],
    output: &io::Output,
    keys: &bpsec::key::KeySet,
) -> anyhow::Result<()> {
    output.append_str(format!("\n## Block {block_number}: "))?;
    match &block.block_type {
        block::Type::Primary => unreachable!(),
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
        output.append_str("Block Flags: None\n")?;
    } else {
        output.append_str("Block Flags:\n")?;

        if block.flags.must_replicate {
            output.append_str("  * Must replicate\n")?;
        }

        if block.flags.report_on_failure {
            output.append_str("  * Report on failure\n")?;
        }

        if block.flags.delete_block_on_failure {
            output.append_str("  * Delete block on failure\n")?;
        }

        if block.flags.delete_bundle_on_failure {
            output.append_str("  * Delete bundle on failure\n")?;
        }

        if let Some(u) = block.flags.unrecognised {
            output.append_str(format!("  * Unrecognised: {u:#x}\n",))?;
        }

        if block.bib.is_some() || block.bcb.is_some() {
            output.append_str("\n")?;
        }
    }

    if let Some(bib) = block.bib {
        output.append_str(format!("Signed by Integrity Block {bib}"))?;

        if let Err(e) = bundle.verify_block(block_number, data, keys) {
            output.append_str(format!(": Error {e}\n"))?;
        } else {
            output.append_str(": ✔\n")?;
        }
    }

    let payload = if let Some(bcb) = block.bcb {
        output.append_str(format!("Encrypted by Security Block {bcb}"))?;

        match bundle.decrypt_block_data(block_number, data, keys) {
            Err(e) => {
                output.append_str(format!(": Error {e}\n"))?;
                None
            }
            Ok(p) => {
                output.append_str(": ✔\n")?;
                Some(p)
            }
        }
    } else {
        bundle
            .block_data(block_number, data, keys)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("Failed to get block data: {e}"))?
    };

    output.append_str("\n")?;

    if let Some(payload) = payload {
        match block.block_type {
            block::Type::Primary => unreachable!(),
            block::Type::PreviousNode => output.append_str(format!(
                "Previous Node: {}\n",
                bundle.previous_node.as_ref().unwrap()
            )),
            block::Type::BundleAge => output.append_str(format!(
                "Bundle Age: {}\n",
                humantime::format_duration(bundle.age.unwrap())
            )),
            block::Type::HopCount => {
                let hop_count = bundle.hop_count.as_ref().unwrap();
                output.append_str(format!(
                    "Hop Count: {} of {}\n",
                    hop_count.count, hop_count.limit
                ))
            }
            block::Type::BlockIntegrity => dump_bib(payload.as_ref(), output),
            block::Type::BlockSecurity => dump_bcb(payload.as_ref(), output),
            block::Type::Payload | block::Type::Unrecognised(_) => {
                dump_unknown(payload.as_ref(), output)
            }
        }
    } else {
        output.append_str(format!(
            "Block Specific Data: {} bytes of encrypted data\n",
            block.data.len()
        ))
    }
}

fn dump_unknown(mut data: &[u8], output: &io::Output) -> anyhow::Result<()> {
    output.append_str("Block Specific Data:")?;

    if data.is_empty() {
        return output.append_str(" None\n");
    }

    if let Ok(s) = str::from_utf8(data)
        && !s.contains(|c: char| c.is_control())
    {
        return output.append_str(format!("\n`{s}`\n"));
    }

    let mut results = Vec::new();
    while let Ok((s, len)) = hardy_cbor::decode::parse_value(data, |v, _, _| {
        let s = format!("{v:?}");

        match v {
            hardy_cbor::decode::Value::Array(array) => {
                array.skip_to_end(16)?;
            }
            hardy_cbor::decode::Value::Map(map) => {
                map.skip_to_end(16)?;
            }
            _ => {}
        }
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
        output.append_str(" Probably CBOR\n")?;
        for s in results {
            output.append_str(format!("`{s}`\n"))?;
        }

        if data.is_empty() {
            return Ok(());
        }

        output.append_str("Followed by")?;
    }

    output.append_str(format!(
        " {} bytes of data in an unrecognized format\n",
        data.len()
    ))
}

fn dump_bcb(data: &[u8], output: &io::Output) -> anyhow::Result<()> {
    let ops = hardy_cbor::decode::parse::<bpsec::bcb::OperationSet>(data)?;
    output.append_str(format!("Security Source: {}\n", ops.source))?;

    match ops.operations.values().next().unwrap() {
        bpsec::bcb::Operation::AES_GCM(op) => {
            output.append_str(format!(
                "Context: BCB-AES-GCM {:?}\n",
                op.parameters.variant
            ))?;
            output.append_str(format!(
                "IV: ({} bits) {}\n",
                op.parameters.iv.len() * 8,
                dump_bytes(&op.parameters.iv),
            ))?;
            if let Some(key) = &op.parameters.key {
                output.append_str(format!("Wrapped Key: {}\n", dump_bytes(key),))?;
            }

            if op.parameters.flags == bpsec::rfc9173::ScopeFlags::NONE {
                output.append_str("Scope Flags: None\n")?;
            } else {
                output.append_str("Scope Flags:\n")?;

                if op.parameters.flags.include_primary_block {
                    output.append_str("  * Include primary block\n")?;
                }

                if op.parameters.flags.include_target_header {
                    output.append_str("  * Include target header\n")?;
                }

                if op.parameters.flags.include_security_header {
                    output.append_str("  * Include security header\n")?;
                }

                if let Some(u) = op.parameters.flags.unrecognised {
                    output.append_str(format!("  * Unrecognised: {u:#x}\n"))?;
                }
            }
        }
        bpsec::bcb::Operation::Unrecognised(_u, op) => {
            output.append_str("Context: Unrecognised Type {u}\n")?;
            for (p, v) in op.parameters.iter() {
                output.append_str(format!("Parameter {p}: {}/n", dump_bytes(v)))?;
            }
        }
    }

    output.append_str("\n")?;

    for (target, op) in ops.operations {
        match op {
            bpsec::bcb::Operation::AES_GCM(op) => {
                if let Some(tag) = &op.results.0 {
                    output.append_str(format!(
                        "Target Block {target} Authentication Tag: {}\n",
                        dump_bytes(tag)
                    ))?;
                } else {
                    output
                        .append_str(format!("Target Block {target} Authentication Tag: None\n"))?;
                }
            }
            bpsec::bcb::Operation::Unrecognised(_u, op) => {
                output.append_str(format!("Target Block: {target}\n"))?;
                for (r, v) in op.results.iter() {
                    output.append_str(format!("Result {}: {}/n", r, dump_bytes(v)))?;
                }
                if !op.results.is_empty() {
                    output.append_str("\n")?;
                }
            }
        }
    }

    Ok(())
}

fn dump_bib(data: &[u8], output: &io::Output) -> anyhow::Result<()> {
    let ops = hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(data)?;
    output.append_str(format!("Security Source: {}\n", ops.source))?;

    match ops.operations.values().next().unwrap() {
        bpsec::bib::Operation::HMAC_SHA2(op) => {
            output.append_str(format!(
                "Context: BIB-HMAC-SHA2 {:?}\n",
                op.parameters.variant
            ))?;
            if let Some(key) = &op.parameters.key {
                output.append_str(format!("Wrapped Key: {}\n", dump_bytes(key),))?;
            }

            if op.parameters.flags == bpsec::rfc9173::ScopeFlags::NONE {
                output.append_str("Scope Flags: None\n")?;
            } else {
                output.append_str("Scope Flags:\n")?;

                if op.parameters.flags.include_primary_block {
                    output.append_str("  * Include primary block\n")?;
                }

                if op.parameters.flags.include_target_header {
                    output.append_str("  * Include target header\n")?;
                }

                if op.parameters.flags.include_security_header {
                    output.append_str("  * Include security header\n")?;
                }

                if let Some(u) = op.parameters.flags.unrecognised {
                    output.append_str(format!("  * Unrecognised: {u:#x}\n"))?;
                }
            }
        }
        bpsec::bib::Operation::Unrecognised(_u, op) => {
            output.append_str("Context: Unrecognised Type {u}\n")?;
            for (p, v) in op.parameters.iter() {
                output.append_str(format!("Parameter {p}: {}/n", dump_bytes(v)))?;
            }
        }
    }

    output.append_str("\n")?;

    for (target, op) in ops.operations {
        match op {
            bpsec::bib::Operation::HMAC_SHA2(op) => {
                output.append_str(format!(
                    "Target Block {target} HMAC: {}\n",
                    dump_bytes(&op.results.0)
                ))?;
            }
            bpsec::bib::Operation::Unrecognised(_u, op) => {
                output.append_str(format!("Target Block: {target}\n"))?;
                for (r, v) in op.results.iter() {
                    output.append_str(format!("Result {r}: {}/n", dump_bytes(v)))?;
                }
                if !op.results.is_empty() {
                    output.append_str("\n")?;
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
