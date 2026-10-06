//! Other programs on this PC that matter to the radio: who holds which serial
//! port or sound device, and which radio programs run (WSJT-X and friends).
//! Read from /proc; S-CTRL never signals or kills any of them.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub cmdline: String,
}

/// Programs that drive radios, and what they are to S-CTRL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioApp {
    Wsjtx,
    Jtdx,
    Js8call,
    Fldigi,
    Gqrx,
    Rigctld,
    Flrig,
    Rev1Diag,
    Openocd,
    StProgrammer,
    Sctrl,
}

impl RadioApp {
    pub fn name(self) -> &'static str {
        match self {
            RadioApp::Wsjtx => "WSJT-X",
            RadioApp::Jtdx => "JTDX",
            RadioApp::Js8call => "JS8Call",
            RadioApp::Fldigi => "fldigi",
            RadioApp::Gqrx => "gqrx",
            RadioApp::Rigctld => "rigctld",
            RadioApp::Flrig => "flrig",
            RadioApp::Rev1Diag => "rev1_diag.py",
            RadioApp::Openocd => "openocd",
            RadioApp::StProgrammer => "STM32_Programmer_CLI",
            RadioApp::Sctrl => "S-CTRL",
        }
    }

    /// Programs that use the radio's CAT port and sound card for digital modes.
    pub fn is_digi_cat(self) -> bool {
        matches!(
            self,
            RadioApp::Wsjtx | RadioApp::Jtdx | RadioApp::Js8call | RadioApp::Fldigi
        )
    }

    fn classify(name: &str, argv0: &str, cmdline: &str) -> Option<Self> {
        let base = Path::new(argv0)
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let name = name.to_lowercase();
        let any = |s: &str| base == s || name == s;
        Some(
            if any("wsjtx") || base.starts_with("wsjtx") && base.ends_with(".appimage") {
                RadioApp::Wsjtx
            } else if any("jtdx") {
                RadioApp::Jtdx
            } else if any("js8call") {
                RadioApp::Js8call
            } else if any("fldigi") {
                RadioApp::Fldigi
            } else if any("gqrx") {
                RadioApp::Gqrx
            } else if any("rigctld") {
                RadioApp::Rigctld
            } else if any("flrig") {
                RadioApp::Flrig
            } else if cmdline.contains("rev1_diag.py") {
                RadioApp::Rev1Diag
            } else if any("openocd") {
                RadioApp::Openocd
            } else if base.starts_with("stm32_programmer_cli") {
                RadioApp::StProgrammer
            } else if any("sctrl") {
                RadioApp::Sctrl
            } else {
                return None;
            },
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub app: RadioApp,
    pub proc: Proc,
    /// WSJT-X's --rig-name, e.g. "SPARTACUS".
    pub rig_name: Option<String>,
}

fn proc_of(pid: u32) -> Option<Proc> {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    let raw = fs::read(dir.join("cmdline")).ok()?;
    let cmdline = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a))
        .collect::<Vec<_>>()
        .join(" ");
    let name = fs::read_to_string(dir.join("comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Some(Proc { pid, name, cmdline })
}

fn pids() -> Vec<u32> {
    fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// Radio programs running now.
pub fn radio_apps() -> Vec<Running> {
    let me = std::process::id();
    let mut out = Vec::new();
    for pid in pids() {
        if pid == me {
            continue;
        }
        let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|&b| b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let argv0 = argv.first().cloned().unwrap_or_default();
        let name = fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let joined = argv.join(" ");
        // Other S-CTRL processes (clients of this one) are not radio programs.
        if let Some(app) = RadioApp::classify(&name, &argv0, &joined).filter(|a| *a != RadioApp::Sctrl) {
            let rig_name = argv
                .iter()
                .find_map(|a| a.strip_prefix("--rig-name=").map(str::to_owned));
            out.push(Running {
                app,
                proc: Proc {
                    pid,
                    name,
                    cmdline: joined,
                },
                rig_name,
            });
        }
    }
    out
}

/// Processes holding `path` open (a tty, a /dev/snd node), this one excluded.
pub fn holders(path: &Path) -> Vec<Proc> {
    let Ok(target) = fs::canonicalize(path) else {
        return Vec::new();
    };
    let me = std::process::id();
    let mut out = Vec::new();
    for pid in pids() {
        if pid == me {
            continue;
        }
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        if fds
            .flatten()
            .any(|fd| fs::read_link(fd.path()).map(|l| l == target).unwrap_or(false))
            && let Some(p) = proc_of(pid)
        {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_programs() {
        assert_eq!(
            RadioApp::classify("wsjtx", "/tmp/.mount_x/usr/bin/wsjtx", ""),
            Some(RadioApp::Wsjtx)
        );
        assert_eq!(
            RadioApp::classify("python3", "python3", "python3 tools/rev1_diag.py term"),
            Some(RadioApp::Rev1Diag)
        );
        assert_eq!(RadioApp::classify("gqrx", "gqrx", "gqrx -c x"), Some(RadioApp::Gqrx));
        assert_eq!(RadioApp::classify("bash", "/bin/bash", "bash"), None);
    }

    #[test]
    fn finds_own_holder_of_a_file() {
        let p = std::env::temp_dir().join(format!("sctrl-holder-{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        let _f = std::fs::File::open(&p).unwrap();
        // This process is excluded by design: nobody else holds it.
        assert!(holders(&p).is_empty());
        std::fs::remove_file(&p).unwrap();
    }
}
