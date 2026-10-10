//! The local socket the running S-CTRL (GUI or `sctrl daemon`) serves, so
//! `sctrl <command>` from a shell or an agent goes through the one process
//! that holds the radio's port instead of fighting it for the port.
//!
//! $XDG_RUNTIME_DIR/sctrl/sctrl.sock, mode 0600. One request per
//! connection: `{"argv": [...]}`; the reply is a stream of `{"out": line}`
//! and one `{"exit": code}`.
//!
//! An S-CTRL on a port named with `--port` serves its own socket,
//! `port-<hash of the port's path>.sock` next to it: a command for that
//! port never reaches the S-CTRL holding the radio on USB, nor the reverse.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub argv: Vec<String>,
    /// The client's build: a daemon from another build refuses.
    #[serde(default)]
    pub build: String,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply {
    Out { out: String },
    Exit { exit: i32 },
}

pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("sctrl").join(socket_name(crate::device::named_port()))
}

fn socket_name(named: Option<&Path>) -> String {
    match named {
        None => "sctrl.sock".into(),
        Some(port) => {
            // FNV-1a: stable across builds, unlike std's hasher.
            let h = port
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                    (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
                });
            format!("port-{h:016x}.sock")
        }
    }
}

/// Is an S-CTRL serving? (A socket file nobody listens on is stale.)
pub async fn running() -> bool {
    UnixStream::connect(socket_path()).await.is_ok()
}

/// Run `argv` in the serving S-CTRL, printing its output; None when none runs.
pub async fn forward(argv: Vec<String>, build: &str) -> Option<i32> {
    let stream = UnixStream::connect(socket_path()).await.ok()?;
    let (r, mut w) = stream.into_split();
    let mut req = serde_json::to_vec(&Request {
        argv,
        build: build.to_string(),
    })
    .ok()?;
    req.push(b'\n');
    w.write_all(&req).await.ok()?;
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        match serde_json::from_str::<Reply>(&l) {
            Ok(Reply::Out { out }) => {
                use std::io::Write;
                // The reader went away (`sctrl ... | head`): stop quietly.
                if writeln!(std::io::stdout(), "{out}").is_err() {
                    return Some(0);
                }
            }
            Ok(Reply::Exit { exit }) => return Some(exit),
            Err(_) => {}
        }
    }
    Some(6)
}

/// A request's output channel: lines until the handler returns its code.
pub type Sink = mpsc::UnboundedSender<String>;

/// Serve until the process ends. `handle(argv, sink)` runs one request;
/// requests from another build of S-CTRL are refused.
pub async fn serve<F, Fut>(build: &'static str, handle: F) -> Result<()>
where
    F: Fn(Vec<String>, Sink) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = i32> + Send + 'static,
{
    let path = socket_path();
    let dir = path.parent().context("socket dir")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    if UnixStream::connect(&path).await.is_ok() {
        anyhow::bail!("another S-CTRL already serves {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let handle = handle.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let Ok(Some(line)) = lines.next_line().await else {
                return;
            };
            let Ok(req) = serde_json::from_str::<Request>(&line) else {
                return;
            };
            if req.build != build {
                let msg = format!(
                    "sctrl: the running S-CTRL (pid {}) is another build than this sctrl; restart it",
                    std::process::id()
                );
                for r in [Reply::Out { out: msg }, Reply::Exit { exit: 3 }] {
                    let mut b = serde_json::to_vec(&r).unwrap_or_default();
                    b.push(b'\n');
                    let _ = w.write_all(&b).await;
                }
                return;
            }
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let job = tokio::spawn(handle(req.argv, tx));
            while let Some(out) = rx.recv().await {
                let mut b = serde_json::to_vec(&Reply::Out { out }).unwrap_or_default();
                b.push(b'\n');
                if w.write_all(&b).await.is_err() {
                    job.abort(); // the client went away (Ctrl-C on `watch`)
                    return;
                }
            }
            let code = job.await.unwrap_or(6);
            let mut b = serde_json::to_vec(&Reply::Exit { exit: code }).unwrap_or_default();
            b.push(b'\n');
            let _ = w.write_all(&b).await;
        });
    }
}

/// Remove the socket when the server stops.
pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_port_has_its_own_socket() {
        assert_eq!(socket_name(None), "sctrl.sock");
        let a = socket_name(Some(Path::new("/tmp/s/tty1")));
        assert!(a.starts_with("port-") && a.ends_with(".sock"), "{a}");
        assert_eq!(a, socket_name(Some(Path::new("/tmp/s/tty1"))));
        assert_ne!(a, socket_name(Some(Path::new("/tmp/t/tty1"))));
    }
}
