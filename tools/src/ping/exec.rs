use super::*;

fn build_payload(args: &Command, seq_no: u32) -> anyhow::Result<Box<[u8]>> {
    let mut builder = hardy_bpv7::builder::Builder::new(
        args.source.as_ref().unwrap().clone(),
        args.destination.clone(),
    );

    if let Some(report_to) = &args.report_to {
        builder = builder.with_report_to(report_to.clone());
    }

    if let Some(lifetime) = &args.lifetime() {
        (lifetime.as_millis() > u64::MAX as u128)
            .then_some(())
            .ok_or(anyhow::anyhow!(
                "Lifetime too long: {}!",
                humantime::format_duration(*lifetime)
            ))?;

        builder = builder.with_lifetime(*lifetime);
    }

    let payload = match args.format {
        Format::Text => payload::Payload::new(seq_no).to_text_fmt().into(),
        Format::Binary => payload::Payload::new(seq_no).to_bin_fmt(),
    };

    Ok(builder
        .add_extension_block(hardy_bpv7::block::Type::Payload)
        .with_flags(hardy_bpv7::block::Flags {
            delete_bundle_on_failure: true,
            ..Default::default()
        })
        .build(&payload)
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .1)
}

async fn start_bpa(args: &Command) -> anyhow::Result<hardy_bpa::bpa::Bpa> {
    let node_id = match args.source.as_ref().unwrap() {
        Eid::LegacyIpn {
            allocator_id,
            node_number,
            ..
        }
        | Eid::Ipn {
            allocator_id,
            node_number,
            ..
        } => Eid::Ipn {
            allocator_id: *allocator_id,
            node_number: *node_number,
            service_number: 0,
        },
        Eid::Dtn { node_name, .. } => Eid::Dtn {
            node_name: node_name.clone(),
            demux: "".into(),
        },
        eid => {
            return Err(anyhow::anyhow!("Invalid source EID '{eid}'"));
        }
    };

    let bpa = hardy_bpa::bpa::Bpa::start(
        &hardy_bpa::config::Config {
            status_reports: true,
            node_ids: [node_id].as_slice().try_into().unwrap(),
            ..Default::default()
        },
        false,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to start BPA: {e}"))?;

    // Try to add some kind of routes

    Ok(bpa)
}

async fn exec_async(args: Command) -> anyhow::Result<()> {
    for seq_no in 0..args.count {
        let payload = build_payload(&args, seq_no)?;
    }

    Ok(())
}

pub fn exec(args: Command) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build tokio runtime: {e}"))?
        .block_on(exec_async(args))
}
