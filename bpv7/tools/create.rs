use super::*;
use hardy_bpv7::eid::Eid;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The source Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    source: Eid,

    /// The destination Endpoint ID (EID) of the bundle
    #[arg(short, long)]
    destination: Eid,

    /// The optional 'Report To' Endpoint ID (EID) of the bundle
    #[arg(short, long = "report-to")]
    report_to: Option<Eid>,

    /// The file to use as payload, use '-' for stdin
    #[arg(short, long)]
    payload: io::Input,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, default_value = "")]
    output: io::Output,

    /// The optional lifetime of the bundle, or 24 hours if not supplied
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let mut builder = hardy_bpv7::builder::Builder::new(self.source, self.destination);

        if let Some(report_to) = self.report_to {
            builder = builder.with_report_to(report_to);
        }

        if let Some(lifetime) = self.lifetime {
            (lifetime.as_millis() > u64::MAX as u128)
                .then_some(())
                .ok_or(anyhow::anyhow!("Lifetime too long: {lifetime}!"))?;

            builder = builder.with_lifetime(lifetime.into());
        }

        self.output.write_all(
            &builder
                .add_extension_block(hardy_bpv7::block::Type::Payload)
                .with_flags(hardy_bpv7::block::Flags {
                    delete_bundle_on_failure: true,
                    ..Default::default()
                })
                .build(&self.payload.read_all()?)
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .1,
        )
    }
}
