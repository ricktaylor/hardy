use bytes::Bytes;
use std::{
    borrow::Cow,
    io::{Read, Write},
    path::PathBuf,
};

#[derive(Debug, Clone)]
pub enum Input {
    StdIn,
    Path(PathBuf),
}

impl Input {
    /// Slurp the entire input into an owned [`Bytes`] buffer (single
    /// `Vec<u8>` allocation, no copy on the `Vec → Bytes` conversion).
    /// Callers that need byte-slice access just deref (`&buf`); callers
    /// that need to mutate in place (e.g. `Chunk::flatten_inplace`)
    /// reclaim a `Vec<u8>` via `Bytes::try_into_mut`.
    pub fn read_all(&self) -> anyhow::Result<Bytes> {
        let vec = match self {
            Self::StdIn => {
                let mut buffer = Vec::new();
                std::io::BufReader::new(std::io::stdin())
                    .read_to_end(&mut buffer)
                    .map_err(|e| anyhow::anyhow!("Failed to read from stdin: {e}"))?;
                buffer
            }
            Self::Path(f) => std::fs::read(f)
                .map_err(|e| anyhow::anyhow!("Failed to read from '{}': {e}", f.display()))?,
        };
        Ok(Bytes::from(vec))
    }

    pub fn filepath<'a>(&'a self) -> Cow<'a, str> {
        match self {
            Self::StdIn => "stdin".into(),
            Self::Path(p) => p.to_string_lossy(),
        }
    }
}

impl std::str::FromStr for Input {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "-" {
            Ok(Input::StdIn)
        } else {
            Ok(Input::Path(PathBuf::from(s)))
        }
    }
}

impl std::fmt::Display for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            Self::StdIn => write!(f, "stdin"),
            Self::Path(p) => write!(f, "{}", p.display()),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Output(Option<PathBuf>);

impl Output {
    pub fn write_all(&self, buf: &[u8]) -> anyhow::Result<()> {
        match &self.0 {
            Some(f) => std::io::BufWriter::new(std::fs::File::create(f).map_err(|e| {
                anyhow::anyhow!("Failed to open output file '{}': {e}", f.display())
            })?)
            .write_all(buf)
            .map_err(|e| anyhow::anyhow!("Failed to write to output file '{}': {e}", f.display())),
            None => std::io::BufWriter::new(std::io::stdout())
                .write_all(buf)
                .map_err(|e| anyhow::anyhow!("Failed to write to stdout: {e}")),
        }
    }

    pub fn write_str<T: AsRef<str>>(&self, buf: T) -> anyhow::Result<()> {
        self.write_all(buf.as_ref().as_bytes())
    }

    pub fn append_str<T: AsRef<str>>(&self, buf: T) -> anyhow::Result<()> {
        let buf = buf.as_ref().as_bytes();
        match &self.0 {
            Some(f) => {
                std::io::BufWriter::new(std::fs::OpenOptions::new().append(true).open(f).map_err(
                    |e| anyhow::anyhow!("Failed to open output file '{}': {e}", f.display()),
                )?)
                .write_all(buf)
                .map_err(|e| {
                    anyhow::anyhow!("Failed to write to output file '{}': {e}", f.display())
                })
            }
            None => std::io::BufWriter::new(std::io::stdout())
                .write_all(buf)
                .map_err(|e| anyhow::anyhow!("Failed to write to stdout: {e}")),
        }
    }
}

impl std::str::FromStr for Output {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            Ok(Self(None))
        } else {
            Ok(Self(Some(PathBuf::from(s))))
        }
    }
}

impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            None => write!(f, "stdout"),
            Some(p) => write!(f, "{}", p.display()),
        }
    }
}
