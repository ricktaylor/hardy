use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The list of bundle files to validate, can include '-' to use stdin.
    files: Vec<io::Input>,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        if self.files.is_empty() {
            return Err(anyhow::anyhow!("No files to validate"));
        }

        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let mut count_failed: usize = 0;
        for input in self.files {
            let bundle = input.read_all()?;

            match hardy_bpv7::bundle::ParsedBundle::parse(&bundle, &key_store) {
                Err(e) => {
                    eprintln!("{}: Failed to parse bundle: {e}", input.filepath());
                    count_failed = count_failed.saturating_add(1);
                }
                Ok(hardy_bpv7::bundle::ParsedBundle { non_canonical, .. }) => {
                    if non_canonical {
                        eprintln!(
                            "{}: Non-canonical, but semantically valid bundle",
                            input.filepath()
                        );
                        count_failed = count_failed.saturating_add(1);
                    }
                }
            }
        }

        (count_failed == 0)
            .then_some(())
            .ok_or(anyhow::anyhow!("{count_failed} files failed to validate"))
    }
}
