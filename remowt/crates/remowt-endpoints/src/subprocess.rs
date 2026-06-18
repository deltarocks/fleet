use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use camino::Utf8PathBuf;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use remowt_link_shared::iroh_tunnel::{TunnelAddr, TunnelDialer, TunnelStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

pub type ProcId = u64;

#[derive(Serialize, Deserialize, Debug)]
pub enum StdioSpec {
	Null,
	Tunnel(TunnelAddr),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum StderrSpec {
	Null,
	Tunnel(TunnelAddr),
	MergeWithStdout,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SpawnSpec {
	pub program: String,
	pub args: Vec<String>,
	pub env: Vec<(String, String)>,
	pub env_clear: bool,
	pub cwd: Option<Utf8PathBuf>,
	pub stdin: StdioSpec,
	pub stdout: StdioSpec,
	pub stderr: StderrSpec,
}

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("spawn failed: {0}")]
	Spawn(String),
	#[error("connect to forwarded socket failed: {0}")]
	Connect(String),
	#[error("no process with that id")]
	NoSuchProcess,
	#[error("MergeWithStdout requires stdout=Socket")]
	BadMerge,
	#[error("invalid signal: {0}")]
	BadSignal(i32),
	#[error("kill failed: {0}")]
	Kill(String),
	#[error("io error: {0}")]
	Io(String),
}

impl From<io::Error> for Error {
	fn from(e: io::Error) -> Self {
		Error::Io(e.to_string())
	}
}

struct ChildState {
	pid: u32,
	exit_rx: watch::Receiver<Option<Option<i32>>>,
}

#[derive(Clone)]
pub struct Subprocess {
	children: Arc<Mutex<HashMap<ProcId, ChildState>>>,
	next_id: Arc<AtomicU64>,
	dialer: Arc<TunnelDialer>,
}

impl Subprocess {
	pub fn new(dialer: Arc<TunnelDialer>) -> Self {
		Self {
			children: Default::default(),
			next_id: Default::default(),
			dialer,
		}
	}
}

#[endpoints(ns = 10)]
impl Subprocess {
	#[endpoints(id = 1)]
	async fn spawn(&self, spec: SpawnSpec) -> Result<ProcId, Error> {
		let SpawnSpec {
			program,
			args,
			env,
			env_clear,
			cwd,
			stdin,
			stdout,
			stderr,
		} = spec;

		if matches!(stderr, StderrSpec::MergeWithStdout) && !matches!(stdout, StdioSpec::Tunnel(_))
		{
			return Err(Error::BadMerge);
		}

		let mut cmd = Command::new(&program);
		cmd.args(&args);
		if env_clear {
			cmd.env_clear();
		}
		for (k, v) in &env {
			cmd.env(k, v);
		}
		if let Some(cwd) = &cwd {
			cmd.current_dir(cwd);
		}
		cmd.stdin(match &stdin {
			StdioSpec::Tunnel(_) => Stdio::piped(),
			StdioSpec::Null => Stdio::null(),
		});
		cmd.stdout(match &stdout {
			StdioSpec::Tunnel(_) => Stdio::piped(),
			StdioSpec::Null => Stdio::null(),
		});
		cmd.stderr(match &stderr {
			StderrSpec::Tunnel(_) | StderrSpec::MergeWithStdout => Stdio::piped(),
			StderrSpec::Null => Stdio::null(),
		});
		cmd.kill_on_drop(false);

		let mut child = cmd.spawn().map_err(|e| Error::Spawn(e.to_string()))?;
		let pid = child
			.id()
			.ok_or_else(|| Error::Spawn("child exited before pid available".to_owned()))?;

		if let StdioSpec::Tunnel(addr) = &stdin {
			let sock = self
				.dialer
				.connect_tunnel(addr)
				.await
				.map_err(|e| Error::Connect(e.to_string()))?;
			let mut stdin_w = child.stdin.take().expect("piped");
			tokio::spawn(async move {
				let (mut sr, _) = tokio::io::split(sock);
				let _ = tokio::io::copy(&mut sr, &mut stdin_w).await;
				let _ = stdin_w.shutdown().await;
			});
		}

		let stdout_handle = child.stdout.take();
		let stderr_handle = child.stderr.take();

		match (&stdout, &stderr, stdout_handle, stderr_handle) {
			(StdioSpec::Tunnel(out_addr), StderrSpec::MergeWithStdout, Some(out), Some(err)) => {
				let sock = self
					.dialer
					.connect_tunnel(out_addr)
					.await
					.map_err(|e| Error::Connect(e.to_string()))?;
				tokio::spawn(merge_to_sock(out, err, sock));
			}
			(StdioSpec::Tunnel(out_addr), _, Some(out), err_opt) => {
				let sock = self
					.dialer
					.connect_tunnel(out_addr)
					.await
					.map_err(|e| Error::Connect(e.to_string()))?;
				tokio::spawn(pump_to_sock(out, sock));
				if let (StderrSpec::Tunnel(err_addr), Some(err)) = (&stderr, err_opt) {
					let err_sock = self
						.dialer
						.connect_tunnel(err_addr)
						.await
						.map_err(|e| Error::Connect(e.to_string()))?;
					tokio::spawn(pump_to_sock(err, err_sock));
				}
			}
			(StdioSpec::Null, StderrSpec::Tunnel(err_addr), _, Some(err)) => {
				let sock = self
					.dialer
					.connect_tunnel(err_addr)
					.await
					.map_err(|e| Error::Connect(e.to_string()))?;
				tokio::spawn(pump_to_sock(err, sock));
			}
			_ => {}
		}

		let (exit_tx, exit_rx) = watch::channel(None);
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		self.children
			.lock()
			.expect("not poisoned")
			.insert(id, ChildState { pid, exit_rx });

		debug!(id, pid, program, "subprocess spawned");
		tokio::spawn(async move {
			let result = child.wait().await;
			let code = match result {
				Ok(status) => status.code(),
				Err(e) => {
					warn!(id, "child.wait failed: {e}");
					None
				}
			};
			let _ = exit_tx.send(Some(code));
		});

		Ok(id)
	}

	#[endpoints(id = 2)]
	async fn wait(&self, id: ProcId) -> Result<Option<i32>, Error> {
		let mut rx = {
			let map = self.children.lock().expect("not poisoned");
			let entry = map.get(&id).ok_or(Error::NoSuchProcess)?;
			entry.exit_rx.clone()
		};
		rx.wait_for(|v| v.is_some())
			.await
			.map_err(|_| Error::Io("exit channel closed".to_owned()))?;
		let code = rx.borrow().flatten();
		self.children.lock().expect("not poisoned").remove(&id);
		Ok(code)
	}

	#[endpoints(id = 3)]
	async fn kill(&self, id: ProcId, signal: i32) -> Result<(), Error> {
		let pid = {
			let map = self.children.lock().expect("not poisoned");
			let entry = map.get(&id).ok_or(Error::NoSuchProcess)?;
			entry.pid
		};
		let sig = Signal::try_from(signal).map_err(|_| Error::BadSignal(signal))?;
		signal::kill(Pid::from_raw(pid as i32), sig).map_err(|e| Error::Kill(e.to_string()))?;
		Ok(())
	}
}

async fn pump_to_sock<R>(mut from: R, sock: TunnelStream)
where
	R: tokio::io::AsyncRead + Unpin,
{
	let (_, mut sw) = tokio::io::split(sock);
	let _ = tokio::io::copy(&mut from, &mut sw).await;
	let _ = sw.shutdown().await;
}

async fn merge_to_sock(mut stdout: ChildStdout, mut stderr: ChildStderr, sock: TunnelStream) {
	let (_, mut sw) = tokio::io::split(sock);
	let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
	let tx_out = tx.clone();
	let out_pump = tokio::spawn(async move {
		let mut buf = vec![0u8; 4096];
		loop {
			match stdout.read(&mut buf).await {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					if tx_out.send(buf[..n].to_vec()).await.is_err() {
						break;
					}
				}
			}
		}
	});
	let err_pump = tokio::spawn(async move {
		let mut buf = vec![0u8; 4096];
		loop {
			match stderr.read(&mut buf).await {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					if tx.send(buf[..n].to_vec()).await.is_err() {
						break;
					}
				}
			}
		}
	});
	while let Some(chunk) = rx.recv().await {
		if sw.write_all(&chunk).await.is_err() {
			break;
		}
	}
	let _ = out_pump.await;
	let _ = err_pump.await;
	let _ = sw.shutdown().await;
}
