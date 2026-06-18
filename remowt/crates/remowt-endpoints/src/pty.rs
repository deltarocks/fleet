use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use nix::libc;
use nix::pty::{openpty, OpenptyResult, Winsize};
use remowt_link_shared::iroh_tunnel::{TunnelAddr, TunnelDialer};
use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{debug, info, warn};

pub type ShellId = u64;

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("openpty failed: {0}")]
	Open(String),
	#[error("failed to spawn shell: {0}")]
	Spawn(String),
	#[error("failed to connect to forwarded socket: {0}")]
	Connect(String),
	#[error("no shell with that id")]
	NoSuchShell,
	#[error("resize failed: {0}")]
	Resize(String),
	#[error("io error: {0}")]
	Io(String),
}

impl From<io::Error> for Error {
	fn from(e: io::Error) -> Self {
		Error::Io(e.to_string())
	}
}

#[derive(Clone)]
pub struct Pty {
	shells: Arc<Mutex<HashMap<ShellId, OwnedFd>>>,
	next_id: Arc<AtomicU64>,
	dialer: Arc<TunnelDialer>,
}

impl Pty {
	pub fn new(dialer: Arc<TunnelDialer>) -> Self {
		Self {
			shells: Default::default(),
			next_id: Default::default(),
			dialer,
		}
	}
}

#[endpoints(ns = 7)]
impl Pty {
	#[endpoints(id = 1)]
	async fn open_shell(
		&self,
		tunnel: TunnelAddr,
		term: String,
		cols: u16,
		rows: u16,
	) -> Result<ShellId, Error> {
		let ws = Winsize {
			ws_row: rows,
			ws_col: cols,
			ws_xpixel: 0,
			ws_ypixel: 0,
		};
		let OpenptyResult { master, slave } =
			openpty(Some(&ws), None).map_err(|e| Error::Open(e.to_string()))?;

		let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());

		let slave_in = slave.try_clone()?;
		let slave_out = slave.try_clone()?;
		let slave_err = slave;

		let mut cmd = tokio::process::Command::new(&shell);
		cmd.env("TERM", &term);
		if let Ok(home) = std::env::var("HOME") {
			cmd.current_dir(home);
		}
		cmd.stdin(Stdio::from(slave_in));
		cmd.stdout(Stdio::from(slave_out));
		cmd.stderr(Stdio::from(slave_err));
		// SAFETY: only async-signal-safe calls (setsid, ioctl) before exec.
		unsafe {
			cmd.pre_exec(|| {
				nix::unistd::setsid().map_err(|e| io::Error::from_raw_os_error(e as i32))?;
				if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
					return Err(io::Error::last_os_error());
				}
				Ok(())
			});
		}

		let mut child = cmd.spawn().map_err(|e| Error::Spawn(e.to_string()))?;

		let resize_fd = master.try_clone()?;
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		self.shells
			.lock()
			.expect("not poisoned")
			.insert(id, resize_fd);

		let sock = match self.dialer.connect_tunnel(&tunnel).await {
			Ok(s) => s,
			Err(e) => {
				self.shells.lock().expect("not poisoned").remove(&id);
				let _ = child.kill().await;
				return Err(Error::Connect(e.to_string()));
			}
		};
		let pty = AsyncPty::new(master)?;

		debug!(id, shell, "shell opened");
		let shells = self.shells.clone();
		tokio::spawn(async move {
			let mut pty = pty;
			let mut sock = sock;
			if let Err(e) = tokio::io::copy_bidirectional(&mut pty, &mut sock).await {
				warn!(id, "shell pump ended: {e}");
			}
			let _ = child.kill().await;
			shells.lock().expect("not poisoned").remove(&id);
			info!(id, "shell closed");
		});

		Ok(id)
	}

	#[endpoints(id = 2)]
	async fn resize(&self, id: ShellId, cols: u16, rows: u16) -> Result<(), Error> {
		let ws = libc::winsize {
			ws_row: rows,
			ws_col: cols,
			ws_xpixel: 0,
			ws_ypixel: 0,
		};
		let shells = self.shells.lock().expect("not poisoned");
		let fd = shells.get(&id).ok_or(Error::NoSuchShell)?;
		// SAFETY: `fd` is a live PTY master
		let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
		if rc < 0 {
			return Err(Error::Resize(io::Error::last_os_error().to_string()));
		}
		Ok(())
	}
}

struct AsyncPty {
	fd: AsyncFd<OwnedFd>,
}

impl AsyncPty {
	fn new(fd: OwnedFd) -> io::Result<Self> {
		let raw = fd.as_raw_fd();
		// SAFETY: standard F_GETFL/F_SETFL round-trip on a valid fd.
		unsafe {
			let flags = libc::fcntl(raw, libc::F_GETFL);
			if flags < 0 {
				return Err(io::Error::last_os_error());
			}
			if libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
				return Err(io::Error::last_os_error());
			}
		}
		Ok(Self {
			fd: AsyncFd::new(fd)?,
		})
	}
}

impl AsyncRead for AsyncPty {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		let this = self.get_mut();
		loop {
			let mut guard = match this.fd.poll_read_ready(cx) {
				Poll::Ready(Ok(g)) => g,
				Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
				Poll::Pending => return Poll::Pending,
			};
			let unfilled = buf.initialize_unfilled();
			let res = guard.try_io(|inner| {
				let fd = inner.get_ref().as_raw_fd();
				// SAFETY: writing into `unfilled`'s own backing storage.
				let n = unsafe { libc::read(fd, unfilled.as_mut_ptr().cast(), unfilled.len()) };
				if n < 0 {
					let err = io::Error::last_os_error();
					if err.raw_os_error() == Some(libc::EIO) {
						Ok(0)
					} else {
						Err(err)
					}
				} else {
					Ok(n as usize)
				}
			});
			match res {
				Ok(Ok(n)) => {
					buf.advance(n);
					return Poll::Ready(Ok(()));
				}
				Ok(Err(e)) => return Poll::Ready(Err(e)),
				Err(_would_block) => continue,
			}
		}
	}
}

impl AsyncWrite for AsyncPty {
	fn poll_write(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		let this = self.get_mut();
		loop {
			let mut guard = match this.fd.poll_write_ready(cx) {
				Poll::Ready(Ok(g)) => g,
				Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
				Poll::Pending => return Poll::Pending,
			};
			let res = guard.try_io(|inner| {
				let fd = inner.get_ref().as_raw_fd();
				// SAFETY: reading from `buf` for `buf.len()` bytes.
				let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
				if n < 0 {
					Err(io::Error::last_os_error())
				} else {
					Ok(n as usize)
				}
			});
			match res {
				Ok(r) => return Poll::Ready(r),
				Err(_would_block) => continue,
			}
		}
	}

	fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Poll::Ready(Ok(()))
	}

	fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Poll::Ready(Ok(()))
	}
}
