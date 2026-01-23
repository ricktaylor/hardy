/*!
I/O utilities for reading and writing files or stdin/stdout
*/

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;

/// Input source - either stdin or a file
#[derive(Debug, Clone)]
pub enum Input {
    Stdin,
    File(PathBuf),
}

impl Input {
    /// Read all bytes from the input source
    pub fn read_all(&self) -> io::Result<Vec<u8>> {
        match self {
            Input::Stdin => {
                let mut buffer = Vec::new();
                io::stdin().read_to_end(&mut buffer)?;
                Ok(buffer)
            }
            Input::File(path) => fs::read(path),
        }
    }

    /// Read all data as a UTF-8 string
    pub fn read_to_string(&self) -> io::Result<String> {
        match self {
            Input::Stdin => {
                let mut buffer = String::new();
                io::stdin().read_to_string(&mut buffer)?;
                Ok(buffer)
            }
            Input::File(path) => fs::read_to_string(path),
        }
    }
}

impl FromStr for Input {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "-" {
            Ok(Input::Stdin)
        } else {
            Ok(Input::File(PathBuf::from(s)))
        }
    }
}

/// Output destination - either stdout or a file
#[derive(Debug, Clone)]
pub enum Output {
    Stdout,
    File(PathBuf),
}

impl Output {
    /// Write all bytes to the output destination
    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        match self {
            Output::Stdout => io::stdout().write_all(data),
            Output::File(path) => fs::write(path, data),
        }
    }

    /// Write a string to the output destination
    pub fn write_str(&self, data: &str) -> io::Result<()> {
        self.write_all(data.as_bytes())
    }
}

impl FromStr for Output {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s == "-" {
            Ok(Output::Stdout)
        } else {
            Ok(Output::File(PathBuf::from(s)))
        }
    }
}
