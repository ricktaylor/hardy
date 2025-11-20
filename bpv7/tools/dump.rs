use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// Pretty-print the output
    #[arg(short, long)]
    pretty: bool,

    /// Path to the location to write the output to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file to dump, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let bundle = self.input.read_all()?;

        let p = hardy_bpv7::bundle::ParsedBundle::parse(&bundle, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

        if p.non_canonical {
            eprintln!(
                "{}: Non-canonical, but semantically valid bundle",
                self.input.filepath()
            );
        }

        let mut json = if self.pretty {
            serde_json::to_string_pretty(&p.bundle)
        } else {
            serde_json::to_string(&p.bundle)
        }
        .map_err(|e| anyhow::anyhow!("Failed to serialize bundle: {e}"))?;
        json.push('\n');

        self.output.write_all(json.as_bytes())
    }
}
