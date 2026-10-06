//! The radio's text console without the LINK: the DIAG personality's port,
//! or a CAT port of firmware older than the LINK (a line ending makes a CAT
//! port the console). Same rules as alt2's rev1_diag.py: one line out,
//! lines back up to `OK` or `ERR ...`, `# ` lines are commentary.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::tty::Tty;

pub struct TextConsole {
    tty: Tty,
    buf: String,
}

impl TextConsole {
    pub async fn open(path: &Path) -> Result<Self> {
        let tty = Tty::open(path)?;
        tty.fresh_session().await?;
        Ok(Self {
            tty,
            buf: String::new(),
        })
    }

    pub async fn cmd(&mut self, line: &str, timeout: Duration) -> Result<Vec<String>> {
        if line.contains(['\r', '\n']) || line.len() > 120 {
            bail!("console line too long or not one line");
        }
        self.tty.write_all(format!("{}\n", line.trim()).as_bytes()).await?;
        let deadline = Instant::now() + timeout;
        let mut lines = Vec::new();
        loop {
            while let Some(i) = self.buf.find(['\n', '\r']) {
                let raw: String = self.buf.drain(..=i).collect();
                let l = raw.trim_end_matches(['\r', '\n']).to_string();
                if l.is_empty() || l.starts_with("# ") || l == "> " || l == ">" {
                    continue;
                }
                if l == "OK" {
                    return Ok(lines);
                }
                if let Some(e) = l.strip_prefix("ERR") {
                    bail!("radio: {}", e.trim());
                }
                // A CAT port answers a stray line with "?;" when the other
                // port holds the console.
                if l == "?;" {
                    bail!("the other CAT port holds the radio's console");
                }
                lines.push(l);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("no reply to `{line}` within {timeout:?}");
            }
            let mut b = [0u8; 1024];
            match tokio::time::timeout(left, self.tty.read(&mut b)).await {
                Ok(r) => {
                    let n = r?;
                    self.buf.push_str(&String::from_utf8_lossy(&b[..n]));
                }
                Err(_) => bail!("no reply to `{line}` within {timeout:?}"),
            }
        }
    }
}

/// `key=value` fields from console lines (the format most commands use).
pub fn fields(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .flat_map(|l| l.split_whitespace())
        .filter_map(|t| t.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}
