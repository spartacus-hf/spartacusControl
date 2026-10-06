//! The radio's SD card over the console: list a folder, read a file. Read
//! only on purpose: the card also holds the radio's settings store and its
//! calibration, which only the radio's own commands may change.
//!
//! `sd ls [path]` prints `dir  0 NAME` / `file SIZE NAME`; `sd get path off
//! len` prints `OFFSET HEX CRC32` lines of up to 32 bytes, each with the
//! CRC-32 (IEEE) of its bytes, so a file arrives whole or not at all.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use crate::link::Link;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SdEntry {
    pub dir: bool,
    pub size: u64,
    pub name: String,
}

/// Bytes asked for per `sd get` (128 console lines).
const CHUNK: u64 = 4096;

pub fn parse_ls(lines: &[String]) -> Vec<SdEntry> {
    lines
        .iter()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let kind = it.next()?;
            let size = it.next()?.parse().ok()?;
            let name = it.collect::<Vec<_>>().join(" ");
            (!name.is_empty() && (kind == "dir" || kind == "file")).then_some(SdEntry {
                dir: kind == "dir",
                size,
                name,
            })
        })
        .collect()
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// One `sd get` line: (offset, bytes), its CRC checked.
pub fn parse_get_line(l: &str) -> Result<(u64, Vec<u8>)> {
    let mut it = l.split_whitespace();
    let (Some(off), Some(hex), Some(crc), None) = (it.next(), it.next(), it.next(), it.next()) else {
        bail!("not a data line: {l:?}");
    };
    let off: u64 = off.parse()?;
    if hex.len() % 2 != 0 {
        bail!("odd hex at {off}");
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<std::result::Result<Vec<u8>, _>>()?;
    let want = u32::from_str_radix(crc, 16)?;
    if crc32(&bytes) != want {
        bail!("CRC mismatch at {off}");
    }
    Ok((off, bytes))
}

/// A path the console takes: relative, `/` between parts, no spaces.
fn check_path(path: &str) -> Result<&str> {
    let p = path.trim_matches('/');
    if p.contains(char::is_whitespace) {
        bail!("{path:?}: the radio's console takes no spaces in paths");
    }
    Ok(p)
}

pub async fn ls(link: &Link, path: &str) -> Result<Vec<SdEntry>> {
    let p = check_path(path)?;
    let lines = link
        .cmd(
            &if p.is_empty() {
                "sd ls".to_string()
            } else {
                format!("sd ls {p}")
            },
            Duration::from_secs(10),
        )
        .await?;
    Ok(parse_ls(&lines))
}

/// Read a whole file, in chunks; `progress(done, total)` after each.
pub async fn get(link: &Link, path: &str, mut progress: impl FnMut(u64, u64)) -> Result<Vec<u8>> {
    let p = check_path(path)?;
    let stat = link.cmd(&format!("sd stat {p}"), Duration::from_secs(5)).await?;
    let size: u64 = stat
        .first()
        .and_then(|l| l.split_whitespace().find_map(|t| t.strip_prefix("size=")))
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| anyhow!("{p}: no such file"))?;
    if stat.first().is_some_and(|l| l.contains("attr=10")) {
        bail!("{p} is a folder");
    }
    let mut out = Vec::with_capacity(size as usize);
    while (out.len() as u64) < size {
        let off = out.len() as u64;
        let len = CHUNK.min(size - off);
        let lines = link
            .cmd(&format!("sd get {p} {off} {len}"), Duration::from_secs(20))
            .await?;
        for l in &lines {
            let (o, bytes) = parse_get_line(l)?;
            if o != out.len() as u64 {
                bail!("{p}: expected offset {}, got {o}", out.len());
            }
            out.extend_from_slice(&bytes);
        }
        if out.len() as u64 == off {
            bail!("{p}: the radio sent nothing at {off}");
        }
        progress(out.len() as u64, size);
    }
    out.truncate(size as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_ieee() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn lists_and_lines() {
        let l: Vec<String> = ["dir  0 LOGS", "file 347 README.TXT", "OK", "file 12 MY NOTE.TXT"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let e = parse_ls(&l);
        assert_eq!(e.len(), 3);
        assert!(e[0].dir && e[0].name == "LOGS");
        assert_eq!((e[1].size, e[1].name.as_str()), (347, "README.TXT"));
        assert_eq!(e[2].name, "MY NOTE.TXT");
        // A line as the radio sent it (README.TXT, 2026-10-06).
        let (o, b) =
            parse_get_line("0 4348495245495820534420434152440D0A46415431362F464154333220776974 EEAFC0FA").unwrap();
        assert_eq!((o, &b[..8]), (0, &b"CHIREIX "[..]));
        let good = format!("32 {} {:08X}", "414243", crc32(b"ABC"));
        assert_eq!(parse_get_line(&good).unwrap(), (32, b"ABC".to_vec()));
        assert!(parse_get_line(&format!("32 414244 {:08X}", crc32(b"ABC"))).is_err());
    }
}
