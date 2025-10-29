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
    pub fn read_all(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::StdIn => {
                let mut buffer = Vec::new();
                std::io::BufReader::new(std::io::stdin())
                    .read_to_end(&mut buffer)
                    .map_err(|e| anyhow::anyhow!("Failed to read from stdin: {e}"))?;
                Ok(buffer)
            }
            Self::Path(f) => std::fs::read(f)
                .map_err(|e| anyhow::anyhow!("Failed to read from '{}': {e}", f.display())),
        }
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
