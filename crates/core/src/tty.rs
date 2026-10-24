//! A USB CDC serial port opened raw: no line discipline, no echo, no CR/LF
//! translation, non-blocking.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;

use anyhow::{Context, Result};
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use nix::sys::termios::{self, BaudRate, ControlFlags, FlushArg, SetArg, SpecialCharacterIndices};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

pub struct Tty {
    fd: AsyncFd<OwnedFd>,
    /// A pseudo-terminal (`--port`): no modem lines to drop.
    pty: bool,
}

impl Tty {
    pub fn open(path: &Path) -> Result<Self> {
        let fd = open(
            path,
            OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("open {}", path.display()))?;
        let mut t = termios::tcgetattr(fd.as_fd()).context("tcgetattr")?;
        termios::cfmakeraw(&mut t);
        // HUPCL: closing drops DTR, which the radio takes as the end of the
        // session (LINK, console, CAT PTT). Some tools clear it, and termios
        // outlives the open: set it every time.
        t.control_flags |= ControlFlags::CLOCAL | ControlFlags::CREAD | ControlFlags::HUPCL;
        // VMIN 1: with O_NONBLOCK an empty port then reads EAGAIN, and 0
        // means hang-up. (VMIN 0, VTIME 0 reads 0 when there is no data.)
        t.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        t.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        // The rate means nothing on a USB CDC port; a sane value for the host.
        let _ = termios::cfsetspeed(&mut t, BaudRate::B115200);
        termios::tcsetattr(fd.as_fd(), SetArg::TCSANOW, &t).context("tcsetattr")?;
        termios::tcflush(fd.as_fd(), FlushArg::TCIOFLUSH).ok();
        Ok(Self {
            fd: AsyncFd::with_interest(fd, Interest::READABLE | Interest::WRITABLE)?,
            pty: crate::device::is_pty(path),
        })
    }

    /// Read what is there, waiting until something is. Ok(0) never happens
    /// on a live port; an error means the device went away.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|inner| nix::unistd::read(inner.get_ref().as_fd(), buf).map_err(std::io::Error::from)) {
                Ok(Ok(0)) => anyhow::bail!("port closed (device gone)"),
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) => return Err(e).context("read"),
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn write_all(&self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| nix::unistd::write(inner.get_ref(), data).map_err(std::io::Error::from)) {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) => return Err(e).context("write"),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// DTR low, then high: the radio sees the port close and open, so
    /// whatever session a previous program left on it (a LINK, the console,
    /// its CAT PTT) ends before this one starts.
    ///
    /// A pseudo-terminal has no DTR (the ioctl fails with ENOTTY): its far
    /// end, such as an emulator, raises DTR while the tty is open,
    /// and the previous program's close already ended its session.
    pub async fn fresh_session(&self) -> Result<()> {
        if self.pty {
            let _ = termios::tcflush(self.fd.get_ref().as_fd(), FlushArg::TCIFLUSH);
            return Ok(());
        }
        let fd = self.raw_fd();
        let dtr: libc::c_int = libc::TIOCM_DTR;
        // SAFETY: TIOCMBIC/TIOCMBIS read one c_int through the pointer.
        if unsafe { libc::ioctl(fd, libc::TIOCMBIC, &dtr) } != 0 {
            return Err(std::io::Error::last_os_error()).context("drop DTR");
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        if unsafe { libc::ioctl(fd, libc::TIOCMBIS, &dtr) } != 0 {
            return Err(std::io::Error::last_os_error()).context("raise DTR");
        }
        // The radio notices within a main-loop pass; give it a few.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        // Anything the old session sent is not ours.
        let _ = termios::tcflush(self.fd.get_ref().as_fd(), FlushArg::TCIFLUSH);
        Ok(())
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd.get_ref().as_raw_fd()
    }
}
