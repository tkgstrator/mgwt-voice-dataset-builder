//! Bounded SHA-256 helpers for approved inputs.
use crate::error::AppError;
use sha2::{Digest, Sha256};
use std::io::Read;

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn sha256_reader(mut reader: impl Read) -> Result<(u64, String), AppError> {
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let size = reader
            .read(&mut chunk)
            .map_err(|e| AppError::validation(format!("read input: {e}")))?;
        if size == 0 {
            break;
        }
        bytes = bytes
            .checked_add(size as u64)
            .ok_or_else(|| AppError::validation("input size overflow"))?;
        hasher.update(&chunk[..size]);
    }
    Ok((bytes, hex::encode(hasher.finalize())))
}

pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
