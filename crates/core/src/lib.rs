//! S-CTRL's engine: finding the radio, talking to it over the LINK, and the
//! host-side watchers (ports, programs). The GUI and the CLI both sit on it.

pub mod console;
pub mod device;
pub mod hostview;
pub mod ipc;
pub mod link;
pub mod procs;
pub mod safety;
pub mod service;
pub mod session;
pub mod tty;
