// src/digest.rs -- thin wrapper for digest computations
// Copyright 2017-2018 the Tectonic Project
// Licensed under the MIT License.

//! Helpers to tidy up the computation of digests in various places.

pub use digest::Digest;
pub use sha2::Sha256 as DigestComputer;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::string::ToString;
pub use twox_hash::XxHash64 as FastDigestComputer;

use crate::errors::{Error, ErrorKind, Result};

// Generic helpers

pub fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .concat()
}

pub fn hex_to_bytes(text: &str, dest: &mut [u8]) -> Result<()> {
    let n = dest.len();
    let text_len = text.len();

    if text_len != 2 * n {
        return Err(ErrorKind::BadLength(2 * n, text_len).into());
    }

    for i in 0..n {
        dest[i] = u8::from_str_radix(&text[i * 2..(i + 1) * 2], 16)?;
    }

    Ok(())
}

// The specific implementation we're using: SHA256.

const N_BYTES: usize = 32;
pub const DIGEST_NAME: &str = "SHA256SUM";
pub const DIGEST_LEN: usize = 64;

pub fn create() -> DigestComputer {
    Default::default()
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DigestData([u8; N_BYTES]);

impl DigestData {
    pub fn zeros() -> DigestData {
        DigestData([0u8; N_BYTES])
    }

    pub fn of_nothing() -> DigestData {
        let dc = create();
        Self::from(dc)
    }

    /// Given a base path, create a child path from this digest's value. The
    /// child path has a subdirectory from the hex value of the first byte of
    /// the digest, then a name consisting of the rest of the hex data. **The
    /// first-byte subdirectory and all parent directories are created when
    /// you call this function!**
    pub fn create_two_part_path(&self, base: &Path) -> Result<PathBuf> {
        let mut p = base.to_path_buf();
        p.push(format!("{:02x}", self.0[0]));
        fs::create_dir_all(&p)?;
        p.push(bytes_to_hex(&self.0[1..]));
        Ok(p)
    }
}

impl ToString for DigestData {
    fn to_string(&self) -> String {
        bytes_to_hex(&self.0)
    }
}

impl FromStr for DigestData {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut result = DigestData::zeros();
        hex_to_bytes(s, &mut result.0)?;
        Ok(result)
    }
}

impl From<DigestComputer> for DigestData {
    fn from(s: DigestComputer) -> DigestData {
        let mut result = DigestData::zeros();
        let res = s.result();
        result.0.copy_from_slice(res.as_slice());
        result
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FastDigestData([u8; 8]);

impl FastDigestData {
    pub fn of_nothing() -> FastDigestData {
        let dc = FastDigestComputer::default();
        Self::from(dc)
    }
}

impl From<FastDigestComputer> for FastDigestData {
    fn from(s: FastDigestComputer) -> FastDigestData {
        let mut result = [0; 8];
        let res = s.result();
        result.copy_from_slice(res.as_slice());
        FastDigestData(result)
    }
}

impl ToString for FastDigestData {
    fn to_string(&self) -> String {
        bytes_to_hex(&self.0)
    }
}

impl FromStr for FastDigestData {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut result = [0; 8];
        hex_to_bytes(s, &mut result)?;
        Ok(FastDigestData(result))
    }
}
