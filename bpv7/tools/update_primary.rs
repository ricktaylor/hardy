use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Update the primary block of a bundle",
    long_about = "Update the primary block of a bundle.\n\n\
        Modifies primary block fields such as lifetime, creation timestamp, \
        source, destination, or report-to EID. At least one update option \
        must be specified.\n\n\
        Note: Updating the primary block will invalidate any BIB signatures \
        that cover it. Use remove-integrity first if needed."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// New lifetime duration (e.g., "1year", "30days", "24h")
    #[arg(short, long)]
    lifetime: Option<humantime::Duration>,

    /// Reset creation timestamp to now (updates both time and sequence number)
    #[arg(short = 't', long = "reset-timestamp")]
    reset_timestamp: bool,

    /// New source EID
    #[arg(short, long)]
    source: Option<hardy_bpv7::eid::Eid>,

    /// New destination EID
    #[arg(short, long)]
    destination: Option<hardy_bpv7::eid::Eid>,

    /// New report-to EID
    #[arg(short, long = "report-to")]
    report_to: Option<hardy_bpv7::eid::Eid>,

    /// Bundle processing flags (comma-separated, replaces existing flags)
    #[arg(short, long, value_delimiter = ',')]
    flags: Vec<flags::ArgBundleFlags>,

    /// CRC type for the primary block
    #[arg(short, long)]
    crc_type: Option<flags::ArgCrcType>,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file to update, '-' to use stdin
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        // Validate that at least one update option is provided
        if self.lifetime.is_none()
            && !self.reset_timestamp
            && self.source.is_none()
            && self.destination.is_none()
            && self.report_to.is_none()
            && self.flags.is_empty()
            && self.crc_type.is_none()
        {
            return Err(anyhow::anyhow!(
                "At least one update option must be specified"
            ));
        }

        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;
        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse_with_keys(&data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        let mut editor = hardy_bpv7::editor::Editor::new(&bundle, &data);

        // Update lifetime if provided
        if let Some(lifetime) = self.lifetime {
            editor = editor
                .with_lifetime(lifetime.into())
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update lifetime: {e}"))?;
        }

        // Reset timestamp if requested
        if self.reset_timestamp {
            editor = editor
                .with_timestamp(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update timestamp: {e}"))?;
        }

        // Update source if provided
        if let Some(source) = self.source {
            editor = editor
                .with_source(source)
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update source: {e}"))?;
        }

        // Update destination if provided
        if let Some(destination) = self.destination {
            editor = editor
                .with_destination(destination)
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update destination: {e}"))?;
        }

        // Update report-to if provided
        if let Some(report_to) = self.report_to {
            editor = editor
                .with_report_to(report_to)
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update report-to: {e}"))?;
        }

        // Update flags if provided
        if let Some(new_flags) = flags::ArgBundleFlags::to_bundle_flags(&self.flags) {
            editor = editor
                .with_bundle_flags(new_flags)
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update flags: {e}"))?;
        }

        // Update CRC type if provided
        if let Some(crc_type) = self.crc_type {
            editor = editor
                .with_bundle_crc_type(crc_type.into())
                .map_err(|(_, e)| anyhow::anyhow!("Failed to update CRC type: {e}"))?;
        }

        let data = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
