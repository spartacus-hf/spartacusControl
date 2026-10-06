//! What else on this PC touches the radio: the CAT program on the other
//! port (WSJT-X and friends), I/Q consumers (gqrx), holders of its ports.
//! Feeds the safety arbiter's Context.

use serde::Serialize;

use crate::device::UsbDevice;
use crate::procs::{self, Proc, RadioApp, Running};

#[derive(Debug, Clone, Default, Serialize)]
pub struct HostView {
    pub apps: Vec<Running>,
    /// Who holds the radio's CAT port (if03).
    pub cat_port_holders: Vec<Proc>,
    /// Who holds S-CTRL's own port (if05 / DIAG if00), S-CTRL excluded.
    pub link_port_holders: Vec<Proc>,
}

impl HostView {
    pub fn gather(dev: Option<&UsbDevice>) -> Self {
        let apps = procs::radio_apps();
        let cat_port_holders = dev
            .and_then(|d| d.cat_port())
            .map(|p| procs::holders(&p))
            .unwrap_or_default();
        let link_port_holders = dev
            .and_then(|d| d.link_port())
            .map(|p| procs::holders(&p))
            .unwrap_or_default();
        Self {
            apps,
            cat_port_holders,
            link_port_holders,
        }
    }

    /// The program that drives the radio over CAT now, if any: whoever holds
    /// the CAT port, else a digital-mode program started for this radio.
    /// `cat0_age_ds` (STATUS) catches CAT traffic from anything else.
    pub fn cat_program(&self, cat0_age_ds: Option<u16>) -> Option<String> {
        if let Some(h) = self.cat_port_holders.first() {
            let app = self
                .apps
                .iter()
                .find(|r| r.proc.pid == h.pid)
                .map(|r| r.app.name().to_string());
            return Some(app.unwrap_or_else(|| format!("{} (pid {})", h.name, h.pid)));
        }
        if let Some(r) = self
            .apps
            .iter()
            .find(|r| r.app.is_digi_cat() && r.rig_name.as_deref() == Some("SPARTACUS"))
        {
            return Some(r.app.name().to_string());
        }
        match cat0_age_ds {
            Some(age) if age < 300 => {
                Some("a CAT program on port 0 (commands in the last 30 s)".into())
            }
            _ => None,
        }
    }

    pub fn iq_consumer(&self) -> Option<String> {
        self.apps
            .iter()
            .find(|r| r.app == RadioApp::Gqrx)
            .map(|r| r.app.name().to_string())
    }
}
