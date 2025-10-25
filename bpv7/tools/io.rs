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
    pub fn read_all(&self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::StdIn => {
                let mut buffer = Vec::new();
                std::io::BufReader::new(std::io::stdin()).read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Self::Path(path) => std::fs::read(path),
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
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "-" {
            Ok(Input::StdIn)
        } else {
            Ok(Input::Path(PathBuf::from(s)))
        }
    }
}

#[derive(Debug, Clone)]
pub struct Output(Option<PathBuf>);

impl Output {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self(path)
    }

    pub fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match &self.0 {
            Some(f) => std::io::BufWriter::new(std::fs::File::create(f)?).write_all(buf),
            None => std::io::BufWriter::new(std::io::stdout()).write_all(buf),
        }
    }

    pub fn filepath<'a>(&'a self) -> Cow<'a, str> {
        match &self.0 {
            None => "stdout".into(),
            Some(p) => p.to_string_lossy(),
        }
    }
}
