//! Shared account layout for onchain sites. Must match program/src/lib.rs.
//!
//! Offset  Size  Field
//! 0       4     magic "SOLW" (set by the program on Init)
//! 4       1     version (1)
//! 5       1     encoding (0 = raw, 1 = brotli)
//! 6       32    authority pubkey (all zeros = frozen / immutable)
//! 38      4     payload length, u32 little-endian
//! 42      32    sha256 of the stored payload (the compressed bytes)
//! 74      ...   payload

use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub const MAGIC: [u8; 4] = *b"SOLW";
pub const HEADER_LEN: usize = 74;
pub const ENC_RAW: u8 = 0;
pub const ENC_BROTLI: u8 = 1;
/// Solana account size cap.
pub const MAX_ACCOUNT: usize = 10 * 1024 * 1024;

pub fn sha256(buf: &[u8]) -> [u8; 32] {
    Sha256::digest(buf).into()
}

pub fn compress(buf: &[u8], encoding: u8) -> Result<Vec<u8>, String> {
    match encoding {
        ENC_RAW => Ok(buf.to_vec()),
        ENC_BROTLI => {
            let mut out = Vec::new();
            {
                // quality 11, 16 MiB window (lgwin 24)
                let mut w = brotli::CompressorWriter::new(&mut out, 4096, 11, 24);
                w.write_all(buf).map_err(|e| e.to_string())?;
            } // dropped here -> stream finished
            Ok(out)
        }
        e => Err(format!("unknown encoding {e}")),
    }
}

pub fn decompress(buf: &[u8], encoding: u8) -> Result<Vec<u8>, String> {
    match encoding {
        ENC_RAW => Ok(buf.to_vec()),
        ENC_BROTLI => {
            let mut out = Vec::new();
            brotli::Decompressor::new(buf, 4096)
                .read_to_end(&mut out)
                .map_err(|e| e.to_string())?;
            Ok(out)
        }
        e => Err(format!("unknown encoding {e}")),
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub version: u8,
    pub encoding: u8,
    pub authority: [u8; 32],
    pub length: u32,
    pub sha256: [u8; 32],
}

pub struct Site<'a> {
    pub header: Header,
    pub payload: &'a [u8],
    /// true if sha256(payload) matches the header hash
    pub hash_ok: bool,
}

pub fn parse_header(acct: &[u8]) -> Result<Header, String> {
    if acct.len() < HEADER_LEN {
        return Err("account too small".into());
    }
    if acct[0..4] != MAGIC {
        return Err("not an initialized site account".into());
    }
    let h = Header {
        version: acct[4],
        encoding: acct[5],
        authority: acct[6..38].try_into().unwrap(),
        length: u32::from_le_bytes(acct[38..42].try_into().unwrap()),
        sha256: acct[42..74].try_into().unwrap(),
    };
    if HEADER_LEN + h.length as usize > acct.len() {
        return Err("declared length exceeds account size".into());
    }
    Ok(h)
}

pub fn read_site(acct: &[u8]) -> Result<Site<'_>, String> {
    let header = parse_header(acct)?;
    let payload = &acct[HEADER_LEN..HEADER_LEN + header.length as usize];
    let hash_ok = sha256(payload) == header.sha256;
    Ok(Site { header, payload, hash_ok })
}
