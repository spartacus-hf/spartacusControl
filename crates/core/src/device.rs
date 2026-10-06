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

use std::fs;
use std::path::{Path, PathBuf};

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
    serial.len() == 12
        && serial
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
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
        (VID_ST, PID_STLINK_V2) | (VID_ST, 0x374B) | (VID_ST, 0x374E) | (VID_ST, 0x374F) => {
            Kind::StLink
        }
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
        let (Some(vid), Some(pid)) = (
            read_hex(&dir.join("idVendor")),
            read_hex(&dir.join("idProduct")),
        ) else {
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
        });
    }
    out
}

pub fn scan() -> Vec<UsbDevice> {
    scan_at(Path::new("/sys/bus/usb/devices"))
}

/// The SPARTACUS to use: the one with `serial` if given, else the only (or
/// first) one attached.
pub fn find_spartacus(serial: Option<&str>) -> Option<UsbDevice> {
    let all: Vec<UsbDevice> = scan().into_iter().filter(UsbDevice::is_spartacus).collect();
    match serial {
        Some(s) => all.into_iter().find(|d| d.serial == s),
        None => all.into_iter().next(),
    }
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
            sp(0x0483, 0x5740, "Chireix", "SPARTACUS REV1", "205D36864D32"),
            Kind::Spartacus {
                personality: Personality::Audio
            }
        );
        assert_eq!(
            sp(
                0x0483,
                0x5742,
                "Chireix",
                "SPARTACUS REV1 DIAG",
                "205D36864D32"
            ),
            Kind::Spartacus {
                personality: Personality::Diag
            }
        );
        assert_eq!(
            sp(
                0x0483,
                0x5741,
                "Chireix",
                "Chireix TX SD Disk",
                "205D36864D32"
            ),
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
            sp(0x0483, 0x5740, "QRP Labs", "SPARTACUS REV1", "205D36864D32"),
            Kind::QrpLabs { .. }
        ));
        // Near misses are never a SPARTACUS.
        assert_eq!(
            sp(0x0483, 0x5740, "Chireix", "SPARTACUS REV2", "205D36864D32"),
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
            sp(0x1234, 0x5740, "Chireix", "SPARTACUS REV1", "205D36864D32"),
            Kind::UnknownChireix
        );
        assert_eq!(
            sp(
                0x0483,
                0x5740,
                "STMicroelectronics",
                "SPARTACUS REV1",
                "205D36864D32"
            ),
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
        write(&d.join("serial"), "205D36864D32\n");
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
            vec![
                (3, PathBuf::from("/dev/ttyACM1")),
                (5, PathBuf::from("/dev/ttyACM2"))
            ]
        );
        // by-id when this machine happens to have one for that tty, the tty itself otherwise
        let lp = sp.link_port().unwrap().to_string_lossy().into_owned();
        assert!(lp.ends_with("ttyACM2") || lp.ends_with("-if05"), "{lp}");
        assert!(all.iter().any(|d| d.kind == Kind::StLink));
        let _ = fs::remove_dir_all(&root);
    }
}
