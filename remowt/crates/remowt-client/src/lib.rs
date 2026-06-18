use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use bifrostlink::declarative::RemoteEndpoints;
use bifrostlink::{Remote, Rpc, Rtt};
use camino::{Utf8Path, Utf8PathBuf};
use remowt_link_shared::iroh_tunnel::{DatagramRouter, IrohBiStream, TunnelAddr};
use remowt_link_shared::plugin::PluginEndpointsClient;
use remowt_link_shared::port::child_port;
use remowt_link_shared::{Address, BifConfig};
use russh::Channel;
use russh::client::{Config, Handle, Handler, Msg, Session, connect};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::check_known_hosts;
use russh::keys::ssh_key::PublicKey;
use tempfile::TempDir;
use tokio::io::AsyncRead;
use tokio::net::UnixListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::{
	fs,
	io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub mod editor;
mod forwarded;
mod shell;
mod ssh_exec;
mod subprocess;

use self::ssh_exec::SshExecChild;
pub use self::subprocess::{RemowtChild, SpawnOptions, StderrMode, StdioMode};
pub use forwarded::{RemowtListener, RemowtStream};
pub use shell::{RemowtShell, RemowtShellResizer};

type Subs = Arc<Mutex<HashMap<Utf8PathBuf, oneshot::Sender<Channel<Msg>>>>>;

fn sh_quote(s: impl AsRef<str>) -> String {
	format!("'{}'", s.as_ref().replace('\'', "'\\''"))
}

const ESCALATORS: [(&str, &[&str]); 2] = [("run0", &["--background=", "--pipe"]), ("sudo", &[])];

pub struct AgentBundle {
	dir: PathBuf,
	hashes: HashMap<String, String>,
}

impl AgentBundle {
	pub fn from_dir(dir: impl Into<PathBuf>) -> Result<Self> {
		let dir = dir.into();
		let hashes_path = dir.join("hashes");
		let raw = std::fs::read_to_string(&hashes_path)
			.with_context(|| format!("reading agent hashes at {}", hashes_path.display()))?;
		let mut hashes = HashMap::new();
		for line in raw.lines() {
			let line = line.trim();
			if line.is_empty() {
				continue;
			}
			let (arch, hash) = line
				.split_once(char::is_whitespace)
				.ok_or_else(|| anyhow!("malformed hashes line: {line:?}"))?;
			hashes.insert(arch.to_owned(), hash.trim().to_owned());
		}
		ensure!(
			!hashes.is_empty(),
			"agent bundle {} has no hashes",
			dir.display()
		);
		Ok(Self { dir, hashes })
	}

	fn binary(&self, arch: &str) -> PathBuf {
		self.dir.join(format!("remowt-agent-{arch}"))
	}

	fn local_binary(&self) -> Result<PathBuf> {
		let arch = env::consts::ARCH;
		let path = self.binary(arch);
		ensure!(
			path.is_file(),
			"no local remowt-agent build for arch {arch} in bundle {}",
			self.dir.display()
		);
		Ok(path)
	}
}

async fn run(sess: &Handle<SshHandler>, cmd: &str) -> Result<(Option<u32>, Vec<u8>)> {
	let ch = sess.channel_open_session().await?;
	ch.exec(true, cmd).await?;

	let mut child = SshExecChild::from_exec(ch);
	drop(child.stdin);
	drain_to_tracing(child.stderr, cmd.to_owned(), true);

	let mut out = Vec::new();
	child.stdout.read_to_end(&mut out).await?;
	let code = child.exit.await.ok().flatten();
	Ok((code, out))
}

async fn run_string_ok(sess: &Handle<SshHandler>, cmd: &str) -> Result<String> {
	let (code, mut out) = run(sess, cmd).await?;
	ensure!(
		code == Some(0),
		"remote command failed (exit {code:?}): {cmd}"
	);
	if !out.is_empty() {
		ensure!(
			out.ends_with(b"\n"),
			"remote command was not newline-terminated: {cmd}: {out:?}"
		);
		out.pop();
	}
	String::from_utf8(out).context("expected utf8 output for command")
}

async fn deploy_agent(sess: &Handle<SshHandler>, bundle: &AgentBundle) -> Result<Utf8PathBuf> {
	debug!("uname -a");
	let arch = run_string_ok(sess, "uname -m").await?;
	let hash = bundle
		.hashes
		.get(&arch)
		.ok_or_else(|| anyhow!("no remowt-agent build for remote arch {arch:?}"))?;

	debug!("get dir");
	let cache = run_string_ok(sess, "echo \"$XDG_CACHE_HOME\"").await?;
	let dir = if cache.is_empty() {
		let home = run_string_ok(sess, "echo \"$HOME\"").await?;
		ensure!(
			!home.is_empty(),
			"remote $HOME and $XDG_CACHE_HOME both empty"
		);
		Utf8PathBuf::from(home).join(".cache/remowt")
	} else {
		Utf8PathBuf::from(cache).join("remowt")
	};
	let path = dir.join(hash);

	debug!("presence");
	let (present, _) = run(sess, &format!("test -x {}", sh_quote(&path))).await?;
	if present != Some(0) {
		let bin = bundle.binary(&arch);
		debug!("read");
		let bytes = fs::read(&bin)
			.await
			.with_context(|| format!("reading agent binary {}", bin.display()))?;
		debug!("upload");
		upload_agent(sess, &dir, &path, bytes).await?;
	}
	Ok(path)
}

async fn upload_agent(
	sess: &Handle<SshHandler>,
	dir: &Utf8Path,
	path: &Utf8Path,
	bytes: Vec<u8>,
) -> Result<()> {
	debug!("mkdirp");
	run_string_ok(sess, &format!("mkdir -p {}", sh_quote(dir))).await?;

	let tmp = dir.join(format!("tmp.{}", Uuid::new_v4()));
	let ch = sess.channel_open_session().await?;
	debug!("cat");
	ch.exec(true, format!("cat > {}", sh_quote(&tmp))).await?;

	let mut child = SshExecChild::from_exec(ch);
	child
		.stdin
		.write_all(&bytes)
		.await
		.context("sending agent binary")?;
	child
		.stdin
		.shutdown()
		.await
		.context("sending agent binary")?;
	let code = child.wait().await;
	ensure!(code == Some(0), "agent upload failed (exit {code:?})");

	debug!("chmod");
	run_string_ok(sess, &format!("chmod 0755 {}", sh_quote(&tmp))).await?;
	run_string_ok(
		sess,
		&format!("mv -f {} {}", sh_quote(&tmp), sh_quote(path)),
	)
	.await?;
	Ok(())
}

pub struct SshHandler {
	host: String,
	port: u16,
	subs: Subs,
}
impl Handler for SshHandler {
	type Error = russh::Error;
	async fn check_server_key(
		&mut self,
		server_public_key: &PublicKey,
	) -> Result<bool, Self::Error> {
		Ok(check_known_hosts(&self.host, self.port, server_public_key)?)
	}
	async fn server_channel_open_forwarded_streamlocal(
		&mut self,
		channel: Channel<Msg>,
		socket_path: &str,
		_session: &mut Session,
	) -> Result<(), Self::Error> {
		let Some(ch) = self
			.subs
			.lock()
			.expect("lock")
			.remove(&Utf8PathBuf::from(socket_path))
		else {
			return Err(russh::Error::WrongChannel);
		};
		let _ = ch.send(channel);
		Ok(())
	}
}

enum Transport {
	Ssh {
		sess: Arc<Handle<SshHandler>>,
		subs: Subs,
		runtime_dir: Utf8PathBuf,
		agent_path: Utf8PathBuf,
	},
	Local {
		agent_path: PathBuf,
		runtime_dir: Utf8PathBuf,
	},
}

struct RemowtInner {
	transport: Transport,
	rpc: Rpc<BifConfig>,
	elevated: tokio::sync::OnceCell<()>,
	#[allow(dead_code)]
	children: Mutex<Vec<tokio::process::Child>>,
	_runtime_tmp: Option<TempDir>,
	user: String,
	iroh: IrohState,
}

#[derive(Default)]
struct IrohState {
	conn: tokio::sync::OnceCell<iroh::endpoint::Connection>,
	#[allow(dead_code)]
	endpoint: Mutex<Option<iroh::Endpoint>>,
	subs: Arc<Mutex<HashMap<u64, oneshot::Sender<RemowtStream>>>>,
	next_token: AtomicU64,
	router: Mutex<Option<Arc<DatagramRouter>>>,
}

#[derive(Clone)]
pub struct Remowt(Arc<RemowtInner>);

pub type RemowtRemote = Remote<BifConfig>;

impl Remowt {
	/// Connect to the remote host over ssh, detect the architecture and deploy the required
	/// agent binary.
	pub async fn connect(host: &str, bundle: &AgentBundle, remowt_user: String) -> Result<Self> {
		let conf = russh_config::parse_home(host)?;
		let port = conf.host_config.port.or(conf.port).unwrap_or(22);
		let hostname = conf
			.host_config
			.hostname
			.clone()
			.unwrap_or_else(|| conf.host_name.clone());
		let user = conf
			.user
			.clone()
			.unwrap_or_else(|| env::var("USER").unwrap_or_else(|_| "root".to_owned()));

		let subs: Subs = Arc::new(Mutex::new(HashMap::new()));
		let config = Config {
			nodelay: true,
			..Config::default()
		};
		let mut sess = connect(
			Arc::new(config),
			(hostname.clone(), port),
			SshHandler {
				host: hostname,
				port,
				subs: subs.clone(),
			},
		)
		.await?;

		let mut agent = AgentClient::connect_env().await?;
		let rsa_hash = sess.best_supported_rsa_hash().await?.flatten();
		let mut authenticated = false;
		for ident in agent.request_identities().await? {
			let AgentIdentity::PublicKey { key, .. } = ident else {
				continue;
			};
			if sess
				.authenticate_publickey_with(user.clone(), key, rsa_hash, &mut agent)
				.await?
				.success()
			{
				authenticated = true;
				break;
			}
		}
		ensure!(authenticated, "ssh authentication failed");

		let sess = Arc::new(sess);

		debug!("deploying agent");
		let agent_path = deploy_agent(&sess, bundle).await?;

		debug!("runtime dir");
		let runtime_dir = remote_runtime_dir(&sess).await?;

		let rpc = Rpc::<BifConfig>::new(Address::User);

		let cmd_chan = sess.channel_open_session().await?;
		debug!("starting agent");
		cmd_chan
			.exec(true, format!("{} real-agent", sh_quote(&agent_path)))
			.await?;

		let child = SshExecChild::from_exec(cmd_chan);
		drain_to_tracing(child.stderr, "agent".to_owned(), true);
		rpc.add_direct(
			Address::Agent,
			child_port(child.stdout, child.stdin),
			Rtt(0),
		);

		let remowt = Self(Arc::new(RemowtInner {
			transport: Transport::Ssh {
				sess,
				subs,
				runtime_dir,
				agent_path,
			},
			rpc,
			elevated: tokio::sync::OnceCell::new(),
			children: Mutex::new(Vec::new()),
			_runtime_tmp: None,
			user: remowt_user,
			iroh: IrohState::default(),
		}));
		remowt.setup_iroh().await;
		Ok(remowt)
	}

	/// "Connect" to the local machine's agent, by starting the agent binary locally.
	pub async fn connect_local(bundle: &AgentBundle, user: String) -> Result<Self> {
		let agent_path = bundle.local_binary()?;
		let mut child = tokio::process::Command::new(&agent_path)
			.arg("real-agent")
			.arg("--local")
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.with_context(|| format!("spawning agent binary {}", agent_path.display()))?;
		let stdin = child.stdin.take().expect("stdin piped");
		let stdout = child.stdout.take().expect("stdout piped");

		let rpc = Rpc::<BifConfig>::new(Address::User);
		rpc.add_direct(Address::Agent, child_port(stdout, stdin), Rtt(0));

		let (runtime_dir, runtime_tmp) = local_runtime_dir()?;

		Ok(Self(Arc::new(RemowtInner {
			transport: Transport::Local {
				agent_path,
				runtime_dir,
			},
			rpc,
			elevated: tokio::sync::OnceCell::new(),
			children: Mutex::new(vec![child]),
			_runtime_tmp: runtime_tmp,
			user,
			iroh: IrohState::default(),
		})))
	}

	/// Get the handle to the underlying russh session handle.
	pub fn ssh(&self) -> Option<Arc<Handle<SshHandler>>> {
		match &self.0.transport {
			Transport::Ssh { sess, .. } => Some(sess.clone()),
			Transport::Local { .. } => None,
		}
	}

	pub fn rpc(&self) -> Rpc<BifConfig> {
		self.0.rpc.clone()
	}

	pub async fn load_plugin(&self, id: u16, name: &str) -> Result<()> {
		let client: PluginEndpointsClient<BifConfig> = self.endpoints();
		client
			.load_plugin(id, name.to_owned())
			.await?
			.map_err(|e| anyhow!("agent failed to load plugin: {e}"))
	}
	pub async fn run0_load_plugin_path(&self, id: u16, path: &str) -> Result<()> {
		self.ensure_escalated().await?;
		let client: PluginEndpointsClient<BifConfig> =
			PluginEndpointsClient::wrap(self.0.rpc.remote(Address::AgentPrivileged));
		client
			.load_plugin_path(id, path.to_owned())
			.await?
			.map_err(|e| anyhow!("privileged agent failed to load plugin: {e}"))
	}
	pub fn plugin_endpoints<R: RemoteEndpoints<BifConfig>>(&self, id: u16) -> R {
		R::wrap(self.0.rpc.remote(Address::Plugin(id)))
	}

	pub fn endpoints<R: RemoteEndpoints<BifConfig>>(&self) -> R {
		R::wrap(self.0.rpc.remote(Address::Agent))
	}
	pub async fn run0_endpoints<R: RemoteEndpoints<BifConfig>>(&self) -> Result<R> {
		self.ensure_escalated().await?;
		Ok(R::wrap(self.0.rpc.remote(Address::AgentPrivileged)))
	}

	async fn ensure_escalated(&self) -> Result<()> {
		self.0
			.elevated
			.get_or_try_init(|| async {
				let (agent_path, local) = match &self.0.transport {
					Transport::Ssh { agent_path, .. } => (agent_path.as_str().to_owned(), false),
					Transport::Local { agent_path, .. } => (
						agent_path
							.to_str()
							.ok_or_else(|| anyhow!("local agent path is not utf-8"))?
							.to_owned(),
						true,
					),
				};

				let (tool, flags) = self.detect_escalation().await?;
				let mut args: Vec<String> = Vec::new();
				args.push("-w".to_owned());
				args.push(tool.to_owned());
				args.extend(flags.iter().copied().map(str::to_owned));
				if tool == "run0" {
					args.push(format!(
						"--unit={}-{}.service",
						self.0.user,
						Uuid::new_v4().simple()
					));
				}
				args.push(agent_path);
				args.push("real-agent".to_owned());
				args.push("--privileged".to_owned());
				if local {
					args.push("--local".to_owned());
				}

				let child = self
					.spawn(SpawnOptions {
						program: "setsid".to_owned(),
						args,
						stdin: StdioMode::Pipe,
						stdout: StdioMode::Pipe,
						stderr: StderrMode::Inherit,
						..Default::default()
					})
					.await
					.context("spawning privileged agent")?;

				let stdin = child
					.stdin
					.ok_or_else(|| anyhow!("privileged agent stdin missing"))?;
				let stdout = child
					.stdout
					.ok_or_else(|| anyhow!("privileged agent stdout missing"))?;

				let port = child_port(stdout, stdin);
				self.0
					.rpc
					.add_direct(Address::AgentPrivileged, port, Rtt(0));
				anyhow::Ok(())
			})
			.await?;
		Ok(())
	}

	async fn detect_escalation(&self) -> Result<(&'static str, &'static [&'static str])> {
		for (tool, flags) in ESCALATORS {
			let probe = self
				.spawn(SpawnOptions {
					program: (*tool).to_owned(),
					args: vec!["--version".to_owned()],
					stdout: StdioMode::Null,
					stderr: StderrMode::Null,
					..Default::default()
				})
				.await;
			if let Ok(child) = probe {
				let _ = child.wait().await;
				return Ok((tool, flags));
			}
		}
		bail!("no escalation tool found")
	}

	/// XDG_RUNTIME_DIR on the remote machine.
	pub fn runtime_dir(&self) -> Utf8PathBuf {
		match &self.0.transport {
			Transport::Ssh { runtime_dir, .. } => runtime_dir.clone(),
			Transport::Local { runtime_dir, .. } => runtime_dir.clone(),
		}
	}

	/// Bind unix listener socket on the remote machine with auto-generated path, returning the path.
	pub async fn bind_runtime_unix(&self, hint: &str) -> Result<(RemowtListener, Utf8PathBuf)> {
		let sock = self
			.runtime_dir()
			.join(format!("remowt-{hint}-{}.sock", Uuid::new_v4()));
		let listener = self.bind_unix(&sock).await?;
		Ok((listener, sock))
	}

	/// Bind unix listener socket on the remote machine on the specified path.
	pub async fn bind_unix(&self, path: &Utf8Path) -> Result<RemowtListener> {
		match &self.0.transport {
			Transport::Ssh { sess, subs, .. } => {
				let (tx, rx) = oneshot::channel();
				subs.lock().expect("lock").insert(path.to_owned(), tx);
				sess.streamlocal_forward(path.to_owned()).await?;
				Ok(RemowtListener::Ssh(rx))
			}
			Transport::Local { .. } => {
				let _ = std::fs::remove_file(path);
				Ok(RemowtListener::Local(
					UnixListener::bind(path)?,
					path.to_owned(),
				))
			}
		}
	}

	/// Bind a data tunnel, preferring the iroh fast path when it is up.
	pub async fn bind_fast_tunnel(
		&self,
		hint: &str,
		escalated: bool,
	) -> Result<(RemowtListener, TunnelAddr)> {
		if !escalated && self.0.iroh.conn.get().is_some() {
			let token = self
				.0
				.iroh
				.next_token
				.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			let (tx, rx) = oneshot::channel();
			self.0.iroh.subs.lock().expect("lock").insert(token, tx);
			return Ok((RemowtListener::Iroh(rx), TunnelAddr::Iroh { token }));
		}
		let (listener, path) = self.bind_runtime_unix(hint).await?;
		Ok((listener, TunnelAddr::Unix(path)))
	}

	async fn setup_iroh(&self) {
		if std::env::var_os("REMOWT_NO_IROH").is_some() {
			debug!("REMOWT_NO_IROH set, skipping iroh fast tunnel");
			return;
		}
		if !matches!(self.0.transport, Transport::Ssh { .. }) {
			return;
		}
		if let Err(e) = self.try_setup_iroh().await {
			warn!("iroh fast tunnel unavailable, using ssh: {e}");
		}
	}

	async fn try_setup_iroh(&self) -> Result<()> {
		use remowt_endpoints::iroh_tunnel::IrohTunnelClient;
		use remowt_link_shared::iroh_tunnel::{REMOWT_ALPN, build_endpoint, ssh_custom_addr};

		let (listener, sock) = self.bind_runtime_unix("iroh-xport").await?;
		let secret = iroh::SecretKey::generate();
		let client_id = secret.public();

		let client: IrohTunnelClient<BifConfig> =
			IrohTunnelClient::wrap(self.0.rpc.remote(Address::Agent));
		let (accepted, agent_id) = tokio::join!(listener.accept(), client.setup(client_id, sock));
		let stream = accepted?;
		let agent_id = agent_id?.map_err(|e| anyhow!("agent iroh setup failed: {e}"))?;

		let ep = build_endpoint(secret, stream, agent_id, false).await?;
		let addr = iroh::EndpointAddr::from_parts(
			agent_id,
			[iroh::TransportAddr::Custom(ssh_custom_addr(agent_id))],
		);
		let conn = ep.connect(addr, REMOWT_ALPN).await?;
		ensure!(conn.remote_id() == agent_id, "iroh peer identity mismatch");
		debug!("iroh fast tunnel established");

		let subs = self.0.iroh.subs.clone();
		let accept_conn = conn.clone();
		tokio::spawn(async move {
			loop {
				match accept_conn.accept_bi().await {
					Ok((send, mut recv)) => {
						let subs = subs.clone();
						tokio::spawn(async move {
							let mut buf = [0u8; 8];
							if tokio::io::AsyncReadExt::read_exact(&mut recv, &mut buf)
								.await
								.is_err()
							{
								return;
							}
							let token = u64::from_be_bytes(buf);
							let tx = subs.lock().expect("lock").remove(&token);
							if let Some(tx) = tx {
								let _ = tx.send(RemowtStream::Iroh(IrohBiStream::new(send, recv)));
							}
						});
					}
					Err(e) => {
						debug!("iroh accept loop ended: {e}");
						break;
					}
				}
			}
		});

		*self.0.iroh.router.lock().expect("lock") = Some(DatagramRouter::spawn(conn.clone()));
		*self.0.iroh.endpoint.lock().expect("lock") = Some(ep);
		let _ = self.0.iroh.conn.set(conn);
		Ok(())
	}

	pub fn datagram_router(&self) -> Option<Arc<DatagramRouter>> {
		self.0.iroh.router.lock().expect("lock").clone()
	}
}

pub(crate) fn drain_to_tracing(
	stream: impl AsyncRead + Unpin + 'static + Send,
	context: String,
	stderr: bool,
) -> JoinHandle<()> {
	tokio::spawn(async move {
		let mut reader = BufReader::new(stream);
		let mut buf = Vec::with_capacity(4096);
		loop {
			buf.clear();
			match reader.read_until(b'\n', &mut buf).await {
				Ok(0) => break,
				Ok(_) => {
					let line = String::from_utf8_lossy(buf.strip_suffix(b"\n").unwrap_or(&buf));
					if stderr {
						warn!(context = %context, "{line}");
					} else {
						info!(context = %context, "{line}");
					}
				}
				Err(e) => {
					warn!(context = %context, "child stdio read failed: {e}");
					break;
				}
			}
		}
	})
}

fn local_runtime_dir() -> Result<(Utf8PathBuf, Option<TempDir>)> {
	if let Ok(dir) = env::var("XDG_RUNTIME_DIR")
		&& !dir.is_empty()
	{
		return Ok((Utf8PathBuf::from(dir), None));
	}
	let tmp = tempfile::Builder::new()
		.prefix("remowt.")
		.rand_bytes(12)
		.tempdir()?;
	let dir = Utf8PathBuf::from_path_buf(tmp.path().to_owned())
		.map_err(|p| anyhow!("temp dir {} is not utf-8", p.display()))?;
	Ok((dir, Some(tmp)))
}

async fn remote_runtime_dir(sess: &Handle<SshHandler>) -> Result<Utf8PathBuf> {
	let dir = run_string_ok(sess, "echo \"$XDG_RUNTIME_DIR\"").await?;
	let dir = dir.trim();
	if dir.is_empty() {
		let tmp = run_string_ok(sess, "mktemp -d remowt.XXXXXXXXXXXX --tmpdir").await?;
		Ok(Utf8PathBuf::from(tmp))
	} else {
		Ok(Utf8PathBuf::from(dir))
	}
}
