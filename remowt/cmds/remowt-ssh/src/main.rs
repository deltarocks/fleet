use std::borrow::Cow;
use std::env::VarError;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::anyhow;
use clap::Parser;
use nix::libc;
use nix::sys::termios::{self, SetArg, Termios};
use remowt_client::editor::SshEditor;
use remowt_client::{AgentBundle, Remowt};
use remowt_link_shared::editor::serve_editor;
use remowt_ui_prompt::auto::AutoPrompter;
use remowt_ui_prompt::bifrost::serve_prompts;
use remowt_ui_prompt::{PrependSourcePrompter, Source};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::signal::unix::{signal, SignalKind};
use tracing::debug;

#[derive(Parser)]
enum Opts {
	/// Connect to remote host with remowt agent.
	Ssh {
		host: String,
		#[arg(long)]
		escalate: bool,
	},
	/// Connect to local host for testing the connectivity.
	Local {
		#[arg(long)]
		escalate: bool,
	},
}

fn agents_dir() -> anyhow::Result<PathBuf> {
	std::env::var_os("REMOWT_AGENTS_DIR")
		.map(PathBuf::from)
		.or_else(|| option_env!("REMOWT_AGENTS_DIR").map(PathBuf::from))
		.ok_or_else(|| anyhow!("no remowt-agents bundle"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_writer(std::io::stderr)
		.without_time()
		.init();
	let opts = Opts::parse();

	let bundle = AgentBundle::from_dir(agents_dir()?)?;
	let (conn, escalate) = match &opts {
		Opts::Ssh { host, escalate } => (
			Remowt::connect(host, &bundle, "remowt-ssh".to_owned()).await?,
			*escalate,
		),
		Opts::Local { escalate } => (
			Remowt::connect_local(&bundle, "remowt-ssh".to_owned()).await?,
			*escalate,
		),
	};
	let mut rpc = conn.rpc();

	serve_prompts(
		&mut rpc,
		PrependSourcePrompter {
			prompter: AutoPrompter::new().await,
			source: match opts {
				Opts::Ssh { host, .. } => vec![Source(Cow::Owned(format!("ssh host: {}", host)))],
				Opts::Local { .. } => vec![],
			},
			description: "".to_owned(),
		},
	);
	if conn.ssh().is_some() {
		serve_editor(&mut rpc, SshEditor { conn: conn.clone() });
	}

	debug!("entering shell");
	run_shell(&conn, escalate).await?;
	debug!("shell ended");

	Ok(())
}

async fn run_shell(conn: &Remowt, escalate: bool) -> anyhow::Result<()> {
	let term = match std::env::var("TERM") {
		Ok(v) => v,
		Err(VarError::NotPresent) => "xterm-256color".to_owned(),
		Err(e) => return Err(e.into()),
	};
	let (cols, rows) = term_size().unwrap_or((80, 24));

	let shell = conn.open_shell(&term, cols, rows, escalate).await?;
	let resizer = shell.resizer();
	let stream = shell.stream;

	let _raw = RawMode::enable();

	if let Ok(mut winch) = signal(SignalKind::window_change()) {
		tokio::spawn(async move {
			while winch.recv().await.is_some() {
				if let Some((cols, rows)) = term_size() {
					let _ = resizer.resize(cols, rows).await;
				}
			}
		});
	}

	let (mut from_remote, mut to_remote) = tokio::io::split(stream);
	let mut stdin = AsyncStdin::new()?;
	let mut stdout = tokio::io::stdout();

	tokio::select! {
		r = tokio::io::copy(&mut from_remote, &mut stdout) => { r?; }
		_ = tokio::io::copy(&mut stdin, &mut to_remote) => {}
	}

	Ok(())
}

struct AsyncStdin {
	fd: AsyncFd<RawFd>,
	original_flags: i32,
}

impl AsyncStdin {
	fn new() -> io::Result<Self> {
		let raw = libc::STDIN_FILENO;
		// SAFETY: F_GETFL/F_SETFL round-trip on a valid fd.
		let original_flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
		if original_flags < 0 {
			return Err(io::Error::last_os_error());
		}
		if unsafe { libc::fcntl(raw, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } < 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(Self {
			fd: AsyncFd::new(raw)?,
			original_flags,
		})
	}
}

impl Drop for AsyncStdin {
	fn drop(&mut self) {
		// SAFETY: restoring the flags we saved on a valid fd.
		unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.original_flags) };
	}
}

impl AsyncRead for AsyncStdin {
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
				let fd = *inner.get_ref();
				// SAFETY: writing into `unfilled`'s own backing storage.
				let n = unsafe { libc::read(fd, unfilled.as_mut_ptr().cast(), unfilled.len()) };
				if n < 0 {
					Err(io::Error::last_os_error())
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

fn term_size() -> Option<(u16, u16)> {
	let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
	let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
	if rc != 0 || ws.ws_col == 0 {
		None
	} else {
		Some((ws.ws_col, ws.ws_row))
	}
}

struct RawMode {
	original: Termios,
}

impl RawMode {
	fn enable() -> Option<Self> {
		let stdin = std::io::stdin();
		// SAFETY: trivial libc call on a borrowed fd.
		if unsafe { libc::isatty(stdin.as_raw_fd()) } != 1 {
			return None;
		}
		let original = termios::tcgetattr(&stdin).ok()?;
		let mut raw = original.clone();
		termios::cfmakeraw(&mut raw);
		termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).ok()?;
		Some(Self { original })
	}
}

impl Drop for RawMode {
	fn drop(&mut self) {
		let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
	}
}
