//! One connection to the radio, whatever it can do: the LINK when the
//! firmware has it, the text console otherwise (DIAG personality, older
//! firmware). Opening refuses anything that is not a SPARTACUS (see
//! device.rs) and never takes a port another program holds.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::console::TextConsole;
use crate::device::{Personality, UsbDevice};
use crate::link::{Link, LinkError, LinkOptions};
use crate::procs;

pub enum Session {
    Link(Link),
    Text { console: TextConsole, why: String },
}

/// Why S-CTRL will not open the radio's port.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("not a SPARTACUS: {0}")]
    NotSpartacus(String),
    #[error("the radio is in USB DISK MODE: no control port until it leaves it (on the radio)")]
    DiskMode,
    #[error("{port} is held by {who}; S-CTRL does not take ports from other programs")]
    Busy { port: String, who: String },
    #[error("the radio shows no S-CTRL port yet (still enumerating?)")]
    NoPort,
}

pub fn port_holders_text(port: &std::path::Path) -> Option<String> {
    let h = procs::holders(port);
    (!h.is_empty()).then(|| {
        h.iter()
            .map(|p| format!("{} (pid {})", p.name, p.pid))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

impl Session {
    pub async fn open(dev: &UsbDevice, opts: LinkOptions) -> Result<Self> {
        let Some(personality) = dev.personality() else {
            return Err(OpenError::NotSpartacus(dev.label()).into());
        };
        if personality == Personality::Disk {
            return Err(OpenError::DiskMode.into());
        }
        let port: PathBuf = dev.link_port().ok_or(OpenError::NoPort)?;
        if let Some(who) = port_holders_text(&port) {
            return Err(OpenError::Busy {
                port: port.display().to_string(),
                who,
            }
            .into());
        }
        if personality == Personality::Diag {
            let mut console = TextConsole::open(&port).await?;
            verify_board(&mut console).await?;
            return Ok(Session::Text {
                console,
                why: "DIAG personality: console only (no LINK there)".into(),
            });
        }
        match Link::connect(&port, opts).await {
            Ok(link) => Ok(Session::Link(link)),
            Err(e) => match e.downcast_ref::<LinkError>() {
                // Older firmware: the same port, as a console (a line ending
                // makes the CAT port a console); the radio already answered,
                // so it is a SPARTACUS console.
                Some(LinkError::NoLink(_)) => {
                    let mut console = TextConsole::open(&port).await?;
                    verify_board(&mut console).await?;
                    Ok(Session::Text {
                        console,
                        why: "firmware without the LINK: basic mode".into(),
                    })
                }
                _ => Err(e),
            },
        }
    }

    pub fn link(&self) -> Option<&Link> {
        match self {
            Session::Link(l) => Some(l),
            Session::Text { .. } => None,
        }
    }

    pub async fn cmd(&mut self, line: &str, timeout: Duration) -> Result<Vec<String>> {
        match self {
            Session::Link(l) => l.cmd(line, timeout).await,
            Session::Text { console, .. } => console.cmd(line, timeout).await,
        }
    }

    pub async fn close(self) {
        if let Session::Link(l) = self {
            l.close().await;
        }
    }
}

/// The console says what board it is: the last check before anything that
/// changes the radio goes to this port.
async fn verify_board(c: &mut TextConsole) -> Result<()> {
    let lines = c.cmd("id", Duration::from_secs(5) * crate::device::patience()).await?;
    let board = lines
        .iter()
        .find_map(|l| l.strip_prefix("board="))
        .ok_or_else(|| anyhow!("no board= in `id`"))?;
    if board != "SPARTACUS REV1" {
        bail!("the console says board={board}: not a SPARTACUS REV1");
    }
    Ok(())
}
