//! Finding radios on USB (sysfs), their serial ports and sound card.
//!
//! VID 0483 is ST's own and many boards use it -- QRP Labs' QMX, QMX+ and
//! QDX among them (0483:a34c "QRP Labs" "QMX Transceiver", no serial). A
//! SPARTACUS is recognised only by the whole tuple its firmware sends: VID,
//! PID, manufacturer "Chireix", the product string of that personality and a
//! 12-digit hex serial (the MCU's UID). Anything else is never opened and
//! never written to: a QMX turns its CAT port into its own terminal menu on a
//! line ending, so a stray `link 1` would type into its menus. Both answer
//! `ID;` with `ID020;`, which is why CAT cannot be the test.
//!
//! Other devices that matter on this bench are listed (QRP Labs radios,
//! ST-LINK, the STM32 ROM DFU loader) so the user sees what is plugged in.
//!
//! `--port` names the LINK port instead (`name_port`): a pseudo-terminal,
//! such as an emulated radio's, or a SPARTACUS's own LINK port. Without it
//! nothing here changes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Serialize;

pub const VID_ST: u16 = 0x0483;
pub const PID_AUDIO: u16 = 0x5740;
pub const PID_MSC: u16 = 0x5741;
pub const PID_DIAG: u16 = 0x5742;
pub const PID_STLINK_V2: u16 = 0x3748;
pub const PID_DFU: u16 = 0xDF11;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Personality {
    /// Sound card + two CDC ports (CAT/console, LINK).
    Audio,
    /// USB DISK MODE: the SD card as a mass-storage device.
    Disk,
    /// The DIAG console only.
    Diag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Kind {
    Spartacus {
        personality: Personality,
    },
    /// A QRP Labs radio (QMX, QMX+, QDX ...): listed, never touched.
    QrpLabs {
        model: String,
    },
    /// Says "Chireix" but is not a SPARTACUS personality this S-CTRL knows
    /// (newer firmware?): listed, never touched.
    UnknownChireix,
    StLink,
    StDfu,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsbDevice {
    pub kind: Kind,
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    /// sysfs device directory, e.g. /sys/bus/usb/devices/7-1
    pub sysfs: PathBuf,
    /// (interface number, /dev/ttyACMn) for every CDC data interface.
    pub ttys: Vec<(u8, PathBuf)>,
    /// ALSA card index, when the device has a sound card.
    pub sound_card: Option<u32>,
    /// A pseudo-terminal named with `--port`: no USB identity, no sound card.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub named: bool,
}

impl UsbDevice {
    pub fn is_spartacus(&self) -> bool {
        matches!(self.kind, Kind::Spartacus { .. })
    }

    pub fn personality(&self) -> Option<Personality> {
        match self.kind {
            Kind::Spartacus { personality } => Some(personality),
            _ => None,
        }
    }

    /// The stable /dev/serial/by-id name of an interface's tty, when udev
    /// made one; the plain /dev/ttyACMn otherwise.
    pub fn port(&self, interface: u8) -> Option<PathBuf> {
        let tty = self.ttys.iter().find(|(i, _)| *i == interface)?.1.clone();
        Some(by_id_for(&tty).unwrap_or(tty))
    }

    /// S-CTRL's port: CDC port 1 (`if05`) of the audio personality, the
    /// console (`if00`) of DIAG. Port 0 (`if03`) is never S-CTRL's.
    pub fn link_port(&self) -> Option<PathBuf> {
        match self.personality()? {
            Personality::Audio => self.port(5),
            Personality::Diag => self.port(0),
            Personality::Disk => None,
        }
    }

    /// WSJT-X's CAT port.
    pub fn cat_port(&self) -> Option<PathBuf> {
        (self.personality()? == Personality::Audio)
            .then(|| self.port(3))
            .flatten()
    }

    pub fn label(&self) -> String {
        if self.named {
            let port = self
                .ttys
                .first()
                .map(|(_, t)| t.display().to_string())
                .unwrap_or_default();
            return format!("SPARTACUS REV1 on the pseudo-terminal {port} (named with --port: no USB, no sound card)");
        }
        match &self.kind {
            Kind::Spartacus { personality } => format!(
                "SPARTACUS REV1 {} ({})",
                match personality {
                    Personality::Audio => "sound card + CAT/LINK ports",
                    Personality::Disk => "USB DISK MODE",
                    Personality::Diag => "DIAG console",
                },
                self.serial
            ),
            Kind::QrpLabs { model } => {
                format!("QRP Labs {model} (another radio: S-CTRL does not touch it)")
            }
            Kind::UnknownChireix => format!(
                "unrecognised Chireix device {:04x}:{:04x} \"{}\" (not opened: newer firmware than this S-CTRL?)",
                self.vid, self.pid, self.product
            ),
            Kind::StLink => "ST-LINK/V2 (SWD bench probe)".into(),
            Kind::StDfu => "STM32 ROM DFU loader".into(),
            Kind::Other => format!(
                "{:04x}:{:04x} {} {}",
                self.vid, self.pid, self.manufacturer, self.product
            ),
        }
    }
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_hex(p: &Path) -> Option<u16> {
    u16::from_str_radix(&read_trim(p)?, 16).ok()
}

/// The firmware's serial: Get_SerialNum, 12 upper-case hex digits.
fn spartacus_serial(serial: &str) -> bool {
    serial.len() == 12 && serial.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

pub fn classify(vid: u16, pid: u16, manufacturer: &str, product: &str, serial: &str) -> Kind {
    if manufacturer.starts_with("QRP Labs") {
        return Kind::QrpLabs {
            model: product.to_string(),
        };
    }
    if manufacturer == "Chireix" {
        let personality = match (vid, pid, product) {
            (VID_ST, PID_AUDIO, "SPARTACUS REV1") => Some(Personality::Audio),
            (VID_ST, PID_MSC, "Chireix TX SD Disk") => Some(Personality::Disk),
            (VID_ST, PID_DIAG, "SPARTACUS REV1 DIAG") => Some(Personality::Diag),
            _ => None,
        };
        return match personality {
            Some(personality) if spartacus_serial(serial) => Kind::Spartacus { personality },
            _ => Kind::UnknownChireix,
        };
    }
    match (vid, pid) {
        (VID_ST, PID_STLINK_V2) | (VID_ST, 0x374B) | (VID_ST, 0x374E) | (VID_ST, 0x374F) => Kind::StLink,
        (VID_ST, PID_DFU) => Kind::StDfu,
        _ => Kind::Other,
    }
}

/// Every USB device under `root` (normally /sys/bus/usb/devices).
pub fn scan_at(root: &Path) -> Vec<UsbDevice> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Devices are "1-2" or "1-2.3"; interfaces carry a ':'; root hubs "usbN".
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let (Some(vid), Some(pid)) = (read_hex(&dir.join("idVendor")), read_hex(&dir.join("idProduct"))) else {
            continue;
        };
        let manufacturer = read_trim(&dir.join("manufacturer")).unwrap_or_default();
        let product = read_trim(&dir.join("product")).unwrap_or_default();
        let serial = read_trim(&dir.join("serial")).unwrap_or_default();
        let kind = classify(vid, pid, &manufacturer, &product, &serial);
        let mut ttys = Vec::new();
        let mut sound_card = None;
        if let Ok(ifs) = fs::read_dir(&dir) {
            for e in ifs.flatten() {
                let ifname = e.file_name().to_string_lossy().into_owned();
                let Some(rest) = ifname.strip_prefix(&format!("{name}:")) else {
                    continue;
                };
                let ifnum = rest
                    .rsplit('.')
                    .next()
                    .and_then(|n| n.parse::<u8>().ok())
                    .unwrap_or(0xFF);
                if let Ok(t) = fs::read_dir(e.path().join("tty")) {
                    for t in t.flatten() {
                        ttys.push((ifnum, PathBuf::from("/dev").join(t.file_name())));
                    }
                }
                if let Ok(s) = fs::read_dir(e.path().join("sound")) {
                    for s in s.flatten() {
                        let n = s.file_name().to_string_lossy().into_owned();
                        if let Some(idx) = n.strip_prefix("card").and_then(|i| i.parse().ok()) {
                            sound_card = Some(idx);
                        }
                    }
                }
            }
        }
        ttys.sort();
        out.push(UsbDevice {
            kind,
            vid,
            pid,
            manufacturer,
            product,
            serial,
            sysfs: dir,
            ttys,
            sound_card,
            named: false,
        });
    }
    out
}

pub fn scan() -> Vec<UsbDevice> {
    scan_at(Path::new("/sys/bus/usb/devices"))
}

/// The SPARTACUS to use: the one on the `--port` if one was named, else
/// the one with `serial` if given, else the only (or first) one attached.
pub fn find_spartacus(serial: Option<&str>) -> Option<UsbDevice> {
    if let Some(port) = named_port() {
        return named_device(port, &scan()).ok();
    }
    let all: Vec<UsbDevice> = scan().into_iter().filter(UsbDevice::is_spartacus).collect();
    match serial {
        Some(s) => all.into_iter().find(|d| d.serial == s),
        None => all.into_iter().next(),
    }
}

static NAMED: OnceLock<PathBuf> = OnceLock::new();

/// `--port`: from now on this process's radio is the one on `port`, found
/// again on every look-up (an emulator restarted in the same directory
/// gives the link a new pseudo-terminal). Set once, at start-up.
pub fn name_port(port: &Path) {
    let abs = std::path::absolute(port).unwrap_or_else(|_| port.to_path_buf());
    let _ = NAMED.set(abs);
}

/// The port named with `--port`, absolute; None when the radio is found on USB.
pub fn named_port() -> Option<&'static Path> {
    NAMED.get().map(PathBuf::as_path)
}

/// How many times longer than for a radio on USB to wait for the radio's
/// answers (`link 1`, HELLO, ACKs, a new dial, mode or page): an emulated
/// radio on a named port runs several times slower than real time. A wait
/// ends as soon as the answer comes. TX limits (STATUS freshness, key
/// times, the heartbeat) never stretch.
pub fn patience() -> u32 {
    if named_port().is_some() { 10 } else { 1 }
}

/// Is `path` (after its symlinks) a Unix98 pseudo-terminal?
pub fn is_pty(path: &Path) -> bool {
    fs::canonicalize(path).is_ok_and(|t| t.starts_with("/dev/pts"))
}

/// The radio on a named port, or why S-CTRL will not open it. `usb` is a
/// scan: a port that is one of its ttys keeps that device's rules (only a
/// SPARTACUS's LINK port, never its CAT port or another radio's); any
/// other port must be a pseudo-terminal, never a plain device node.
pub fn named_device(port: &Path, usb: &[UsbDevice]) -> Result<UsbDevice, String> {
    let target = fs::canonicalize(port).map_err(|e| format!("{}: {e}", port.display()))?;
    for d in usb {
        let Some((i, _)) = d
            .ttys
            .iter()
            .find(|(_, t)| fs::canonicalize(t).is_ok_and(|t| t == target))
        else {
            continue;
        };
        let link = match d.personality() {
            Some(Personality::Audio) => Some(5),
            Some(Personality::Diag) => Some(0),
            _ => None,
        };
        if d.is_spartacus() && link == Some(*i) {
            return Ok(d.clone());
        }
        return Err(format!(
            "{} is interface if{i:02} of {}: S-CTRL opens only a SPARTACUS's LINK port",
            port.display(),
            d.label()
        ));
    }
    if !target.starts_with("/dev/pts") {
        return Err(format!(
            "{} is neither a pseudo-terminal nor a SPARTACUS's LINK port on USB",
            port.display()
        ));
    }
    Ok(UsbDevice {
        kind: Kind::Spartacus {
            personality: Personality::Audio,
        },
        vid: 0,
        pid: 0,
        manufacturer: String::new(),
        product: String::new(),
        // Never a real radio's (12 upper-case hex digits): nothing keyed by
        // serial, such as the PC speaker's PipeWire nodes, can match it.
        serial: "pty".into(),
        sysfs: PathBuf::new(),
        ttys: vec![(5, port.to_path_buf())],
        sound_card: None,
        named: true,
    })
}

/// /dev/serial/by-id entry that points at `tty`.
pub fn by_id_for(tty: &Path) -> Option<PathBuf> {
    let dir = Path::new("/dev/serial/by-id");
    for e in fs::read_dir(dir).ok()?.flatten() {
        let target = fs::canonicalize(e.path()).ok()?;
        if target == tty {
            return Some(e.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn only_the_whole_tuple_is_a_spartacus() {
        let sp = |vid, pid, m: &str, p: &str, s: &str| classify(vid, pid, m, p, s);
        assert_eq!(
            sp(0x0483, 0x5740, "Chireix", "SPARTACUS REV1", "1A2B3C4D5E6F"),
            Kind::Spartacus {
                personality: Personality::Audio
            }
        );
        assert_eq!(
            sp(0x0483, 0x5742, "Chireix", "SPARTACUS REV1 DIAG", "1A2B3C4D5E6F"),
            Kind::Spartacus {
                personality: Personality::Diag
            }
        );
        assert_eq!(
            sp(0x0483, 0x5741, "Chireix", "Chireix TX SD Disk", "1A2B3C4D5E6F"),
            Kind::Spartacus {
                personality: Personality::Disk
            }
        );
        // QMX / QMX+ / QDX: ST's VID too, QRP Labs strings, no serial.
        let qmx = sp(0x0483, 0xA34C, "QRP Labs", "QMX Transceiver", "");
        assert_eq!(
            qmx,
            Kind::QrpLabs {
                model: "QMX Transceiver".into()
            }
        );
        assert!(matches!(
            sp(0x0483, 0xA34C, "QRP Labs", "QMX+ Transceiver", ""),
            Kind::QrpLabs { .. }
        ));
        // Even a QRP Labs device on SPARTACUS's own IDs is QRP Labs.
        assert!(matches!(
            sp(0x0483, 0x5740, "QRP Labs", "SPARTACUS REV1", "1A2B3C4D5E6F"),
            Kind::QrpLabs { .. }
        ));
        // Near misses are never a SPARTACUS.
        assert_eq!(
            sp(0x0483, 0x5740, "Chireix", "SPARTACUS REV2", "1A2B3C4D5E6F"),
            Kind::UnknownChireix
        );
        assert_eq!(
            sp(0x0483, 0x5740, "Chireix", "SPARTACUS REV1", ""),
            Kind::UnknownChireix
        );
        assert_eq!(
            sp(0x0483, 0x5740, "Chireix", "SPARTACUS REV1", "205d36864d32"),
            Kind::UnknownChireix
        );
        assert_eq!(
            sp(0x1234, 0x5740, "Chireix", "SPARTACUS REV1", "1A2B3C4D5E6F"),
            Kind::UnknownChireix
        );
        assert_eq!(
            sp(0x0483, 0x5740, "STMicroelectronics", "SPARTACUS REV1", "1A2B3C4D5E6F"),
            Kind::Other
        );
        assert_eq!(
            sp(0x0483, 0x3748, "STMicroelectronics", "STM32 STLink", "x"),
            Kind::StLink
        );
    }

    #[test]
    fn a_qmx_never_gets_a_port() {
        let root = std::env::temp_dir().join(format!("sctrl-sysfs-qmx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = root.join("1-4");
        write(&d.join("idVendor"), "0483\n");
        write(&d.join("idProduct"), "a34c\n");
        write(&d.join("manufacturer"), "QRP Labs\n");
        write(&d.join("product"), "QMX Transceiver\n");
        fs::create_dir_all(d.join("1-4:1.0/sound/card4")).unwrap();
        fs::create_dir_all(d.join("1-4:1.3/tty/ttyACM0")).unwrap();
        let all = scan_at(&root);
        assert_eq!(all.len(), 1);
        assert!(!all[0].is_spartacus());
        assert_eq!(all[0].link_port(), None);
        assert_eq!(all[0].cat_port(), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_spartacus_ports_and_card() {
        let root = std::env::temp_dir().join(format!("sctrl-sysfs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = root.join("7-1");
        write(&d.join("idVendor"), "0483\n");
        write(&d.join("idProduct"), "5740\n");
        write(&d.join("manufacturer"), "Chireix\n");
        write(&d.join("product"), "SPARTACUS REV1\n");
        write(&d.join("serial"), "1A2B3C4D5E6F\n");
        fs::create_dir_all(d.join("7-1:1.0/sound/card3")).unwrap();
        fs::create_dir_all(d.join("7-1:1.3/tty/ttyACM1")).unwrap();
        fs::create_dir_all(d.join("7-1:1.5/tty/ttyACM2")).unwrap();
        let s = root.join("3-4");
        write(&s.join("idVendor"), "0483\n");
        write(&s.join("idProduct"), "3748\n");
        write(&s.join("product"), "STM32 STLink\n");
        let all = scan_at(&root);
        let sp = all.iter().find(|d| d.is_spartacus()).unwrap();
        assert_eq!(sp.personality(), Some(Personality::Audio));
        assert_eq!(sp.sound_card, Some(3));
        assert_eq!(
            sp.ttys,
            vec![(3, PathBuf::from("/dev/ttyACM1")), (5, PathBuf::from("/dev/ttyACM2"))]
        );
        // by-id when this machine happens to have one for that tty, the tty itself otherwise
        let lp = sp.link_port().unwrap().to_string_lossy().into_owned();
        assert!(lp.ends_with("ttyACM2") || lp.ends_with("-if05"), "{lp}");
        assert!(all.iter().any(|d| d.kind == Kind::StLink));
        // A device found on USB serialises as it always did.
        assert!(!serde_json::to_string(sp).unwrap().contains("named"));
        let _ = fs::remove_dir_all(&root);
    }

    /// A pseudo-terminal and its slave's path; the slave lives while the master does.
    fn pty() -> (nix::pty::PtyMaster, PathBuf) {
        use nix::fcntl::OFlag;
        let m = nix::pty::posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY).unwrap();
        nix::pty::grantpt(&m).unwrap();
        nix::pty::unlockpt(&m).unwrap();
        let p = PathBuf::from(nix::pty::ptsname_r(&m).unwrap());
        (m, p)
    }

    #[test]
    fn a_named_pty_is_a_radio_without_usb() {
        let dir = std::env::temp_dir().join(format!("sctrl-named-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (_m, slave) = pty();
        // A symlink to the pty, as an emulator links its ports by name.
        let link = dir.join("tty1");
        std::os::unix::fs::symlink(&slave, &link).unwrap();
        assert!(is_pty(&link));
        let d = named_device(&link, &[]).unwrap();
        assert!(d.named && d.is_spartacus());
        assert_eq!(d.personality(), Some(Personality::Audio));
        assert_eq!(d.link_port(), Some(link.clone()));
        assert_eq!(d.cat_port(), None);
        assert_eq!(d.sound_card, None);
        assert!(!spartacus_serial(&d.serial), "{}", d.serial);
        assert!(d.label().contains("--port"), "{}", d.label());
        assert!(serde_json::to_string(&d).unwrap().contains("\"named\":true"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_named_port_every_wait_is_as_before() {
        // No test names a port: that is process-wide, set once by `--port`.
        assert_eq!(named_port(), None);
        assert_eq!(patience(), 1);
    }

    #[test]
    fn a_named_port_is_never_a_plain_device_or_another_radios() {
        assert!(!is_pty(Path::new("/dev/null")));
        let e = named_device(Path::new("/dev/null"), &[]).unwrap_err();
        assert!(e.contains("neither a pseudo-terminal"), "{e}");
        assert!(named_device(Path::new("/nonexistent/sctrl-tty"), &[]).is_err());

        // A radio on USB keeps its rules when its tty is named: only its
        // LINK port opens; its CAT port, or a QMX's port, never does.
        let root = std::env::temp_dir().join(format!("sctrl-named-usb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = root.join("7-1");
        write(&d.join("idVendor"), "0483\n");
        write(&d.join("idProduct"), "5740\n");
        write(&d.join("manufacturer"), "Chireix\n");
        write(&d.join("product"), "SPARTACUS REV1\n");
        write(&d.join("serial"), "1A2B3C4D5E6F\n");
        let q = root.join("1-4");
        write(&q.join("idVendor"), "0483\n");
        write(&q.join("idProduct"), "a34c\n");
        write(&q.join("manufacturer"), "QRP Labs\n");
        write(&q.join("product"), "QMX Transceiver\n");
        let mut usb = scan_at(&root);
        let (cat, link, qmx) = (root.join("ttyACM1"), root.join("ttyACM2"), root.join("ttyACM0"));
        for t in [&cat, &link, &qmx] {
            write(t, "");
        }
        for u in usb.iter_mut() {
            u.ttys = if u.is_spartacus() {
                vec![(3, cat.clone()), (5, link.clone())]
            } else {
                vec![(3, qmx.clone())]
            };
        }
        let sp = named_device(&link, &usb).unwrap();
        assert!(!sp.named && sp.serial == "1A2B3C4D5E6F");
        let e = named_device(&cat, &usb).unwrap_err();
        assert!(e.contains("if03") && e.contains("LINK port"), "{e}");
        let e = named_device(&qmx, &usb).unwrap_err();
        assert!(e.contains("QRP Labs"), "{e}");
        let _ = fs::remove_dir_all(&root);
    }
}
