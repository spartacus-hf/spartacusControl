//! The radio's text console without the LINK: the DIAG personality's port,
//! or a CAT port of firmware older than the LINK (a line ending makes a CAT
//! port the console). One line out, lines back up to `OK` or `ERR ...`,
//! `# ` lines are commentary.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::device::{self, Personality, UsbDevice};
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

/// Leave the DIAG personality for the sound card + CAT/LINK ports (the
/// console's `exit`) and wait for them. Only when asked (`sctrl usb audio`,
/// the window's button): a console nobody else holds, a radio receiving.
pub async fn leave_diag(dev: &UsbDevice) -> Result<()> {
    let Some(port) = dev.link_port().filter(|_| dev.personality() == Some(Personality::Diag)) else {
        bail!("the radio is not in its DIAG personality");
    };
    if let Some(who) = crate::session::port_holders_text(&port) {
        bail!("{} is held by {who}", port.display());
    }
    let mut c = TextConsole::open(&port).await?;
    let id = c.cmd("id", Duration::from_secs(3)).await?;
    let state = id.iter().find_map(|l| l.strip_prefix("state=")).unwrap_or("unknown");
    if !matches!(state, "RECEIVE" | "PROTECTION") {
        bail!("the radio is in {state}; wait for receive");
    }
    let _ = c.cmd("exit", Duration::from_secs(1)).await; // the port goes away with the reply
    drop(c);
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let serial = dev.serial.clone();
        let back = tokio::task::spawn_blocking(move || device::find_spartacus(Some(&serial)))
            .await
            .ok()
            .flatten();
        if back.is_some_and(|d| d.personality() == Some(Personality::Audio) && d.link_port().is_some()) {
            return Ok(());
        }
    }
    bail!("the radio did not come back as a sound card within 10 s")
}
