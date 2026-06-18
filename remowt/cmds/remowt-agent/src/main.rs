use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs::Permissions;
use std::future::pending;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use bifrostlink::declarative::RemoteEndpoints;
use bifrostlink::Rpc;
use bifrostlink_ports::stdio::from_stdio;
use bifrostlink_ports::unix_socket::from_socket;
use clap::Parser;
use remowt_endpoints::{
	forward::Forward, fs::Fs, iroh_tunnel::IrohTunnel, nix_daemon::NixDaemon, pty::Pty,
	subprocess::Subprocess, systemd::Systemd,
};
use remowt_link_shared::iroh_tunnel::TunnelDialer;
use remowt_link_shared::{editor::EditorEndpointsClient, Address, BifConfig};
use remowt_polkit_shared::{emphasize, Identity, PidDisplay};
use remowt_ui_prompt::bifrost::PromptEndpointsClient;
use remowt_ui_prompt::rofi::RofiPrompter;
use remowt_ui_prompt::{PrependSourcePrompter, Prompter, Source};
use tokio::fs;
use tokio::net::UnixStream;
use tokio::runtime::Builder;
use tokio::task::AbortHandle;
use tracing::{debug, trace};
use zbus::fdo;
use zbus::zvariant::{OwnedValue, Str};
use zbus::{interface, Connection};
use zbus_polkit::policykit1::Subject;

use self::helper::{Helper, SocketHelper, SuidHelper};

pub mod askpass;
pub mod bus;
pub mod editor;
pub mod helper;

struct CancelTaskOnDrop {
	tasks: Arc<Mutex<HashMap<String, AbortHandle>>>,
	handle: String,
}
impl Drop for CancelTaskOnDrop {
	fn drop(&mut self) {
		debug!("cancel on drop");
		if let Some(task) = self
			.tasks
			.lock()
			.expect("not poisoned")
			.remove(&self.handle)
		{
			task.abort();
		}
	}
}

struct Agent<H, P> {
	tasks: Arc<Mutex<HashMap<String, AbortHandle>>>,
	helper: H,
	prompter: P,
}
impl<H, P> Agent<H, P> {
	fn new(helper: H, prompter: P) -> Self {
		Agent {
			tasks: Arc::new(Mutex::new(HashMap::new())),
			helper,
			prompter,
		}
	}
}

#[interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl<H, P> Agent<H, P>
where
	H: Helper + Clone + Send + Sync + 'static,
	P: Prompter + Clone + Send + Sync + 'static,
{
	/// BeginAuthentication method
	#[allow(clippy::too_many_arguments)]
	async fn begin_authentication(
		&self,
		action_id: String,
		message: String,
		_icon_name: String,
		mut details: BTreeMap<String, String>,
		cookie: String,
		identities: Vec<Identity>,
	) -> zbus::fdo::Result<()> {
		use std::fmt::Write;
		debug!("begin auth");
		let _cancel_guard = Arc::new(OnceLock::new());
		let task = {
			let helper = self.helper.clone();
			let prompter = self.prompter.clone();
			let cookie = cookie.clone();
			let _cancel_guard = _cancel_guard.clone();
			tokio::task::spawn(async move {
				let _cancel_guard = _cancel_guard.clone();
				trace!("conversation task");
				let mut description = format!("{message}\n\n<b>Action id:</b> {action_id}",);
				if let Some(subject) = details.remove("polkit.caller-pid") {
					let _ = write!(description, "\n<b>Caller:</b> ");
					if let Ok(pid) = subject.parse::<u32>() {
						let _ = write!(description, "{}", PidDisplay(pid));
					} else {
						let _ = write!(description, "{}", emphasize("invalid pid"));
					}
				}
				if let Some(subject) = details.remove("polkit.subject-pid") {
					let _ = write!(description, "\n<b>Subject:</b> ");
					if let Ok(pid) = subject.parse::<u32>() {
						let _ = write!(description, "{}", PidDisplay(pid));
					} else {
						let _ = write!(description, "{}", emphasize("invalid pid"));
					}
				}
				let mut prompter = PrependSourcePrompter {
					source: vec![Source(Cow::Borrowed("polkit agent"))],
					description: description.clone(),
					prompter,
				};

				let identity_displays: Vec<String> =
					identities.iter().map(|v| v.to_string()).collect();
				let identity_displays: Vec<&str> =
					identity_displays.iter().map(|v| v.as_str()).collect();
				debug!("choose identity");
				let choosen_identity = match identity_displays.len() {
					0 => {
						return Err(fdo::Error::AuthFailed(
							"no identity to authenticate as".to_owned(),
						))
					}
					1 => 0,
					_ => {
						prompter
							.prompt_enum(
								"Identity",
								"Select identity to use for polkit authorization",
								&identity_displays,
								&[],
							)
							.await?
					}
				};
				debug!("identity chosen");

				let _ = write!(
					description,
					"\n<b>Identity:</b> {}",
					identities[choosen_identity as usize]
				);
				prompter.description = description;

				prompter.source.push(Source(Cow::Borrowed("polkit daemon")));

				helper
					.help_me(
						&cookie,
						prompter,
						identities[choosen_identity as usize].clone(),
					)
					.await
					.map_err(|e| fdo::Error::Failed(e.to_string()))?;
				// let connection = Connection::system().await?;
				// let helper = PolkitHelperProxy::new(&connection).await?;

				Ok(())
			})
		};
		self.tasks
			.lock()
			.unwrap()
			.insert(cookie.clone(), task.abort_handle());
		debug!("abort handle stored");
		let _ = _cancel_guard.set(CancelTaskOnDrop {
			tasks: self.tasks.clone(),
			handle: cookie.clone(),
		});

		let _ = task.await;

		Ok(())
	}

	/// CancelAuthentication method
	async fn cancel_authentication(&self, cookie: &str) -> zbus::fdo::Result<()> {
		debug!("auth cancelled");
		if let Some(abort) = self.tasks.lock().unwrap().remove(cookie) {
			debug!("abort handle found");
			abort.abort();
		}
		// debug!("Authentication cancled ! {cookie}");
		Ok(())
	}
}

const OBJ_PATH: &str = "/org/freedesktop/PolicyKit1/AuthenticationAgent";

#[derive(Parser)]
enum Opts {
	AskPass {
		prompt: String,
		description: String,
	},
	Editor {
		/// Argument to nvim
		path: String,
	},
	/// Expose a remote TCP/UDP address on the local (User) machine, printing the local port.
	Forward {
		/// Protocol: `tcp` (ssh or iroh) or `udp` (iroh-only).
		proto: ForwardProto,
		/// Remote socket address to expose locally, e.g. `127.0.0.1:8080`.
		addr: String,
	},
	RealAgent {
		#[arg(long)]
		path: Option<PathBuf>,
		/// Expect own address to be AgentPrivileged, skip installing polkit agent
		#[arg(long)]
		privileged: bool,
		#[arg(long)]
		local: bool,
	},
	LocalAgent,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ForwardProto {
	Tcp,
	Udp,
}

fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_writer(io::stderr)
		.without_time()
		.init();
	let opts = Opts::parse();

	let runtime = Builder::new_current_thread().enable_all().build()?;

	match opts {
		Opts::AskPass {
			prompt,
			description,
		} => runtime.block_on(askpass::ask(&prompt, description)),
		Opts::LocalAgent => runtime.block_on(main_real()),
		Opts::Editor { path } => runtime.block_on(editor::edit(path)),
		Opts::Forward { proto, addr } => {
			runtime.block_on(editor::forward(matches!(proto, ForwardProto::Udp), addr))
		}
		Opts::RealAgent {
			path,
			privileged,
			local,
		} => runtime.block_on(main_real_agent(path, privileged, local)),
	}
}
async fn main_real() -> anyhow::Result<()> {
	let system_conn = Connection::system().await?;
	let helper = SocketHelper {
		fallback: SuidHelper,
	};
	register_auth_agent(&system_conn, Agent::new(helper, RofiPrompter)).await?;

	let session_conn = Connection::session().await?;
	askpass::serve(&session_conn, RofiPrompter).await?;

	let _keep_alive = (system_conn, session_conn);
	pending().await
}
async fn main_real_agent(
	path: Option<PathBuf>,
	privileged: bool,
	local: bool,
) -> anyhow::Result<()> {
	let address = if privileged {
		Address::AgentPrivileged
	} else {
		Address::Agent
	};
	let mut rpc = Rpc::<BifConfig>::new(address);

	let dialer = Arc::new(TunnelDialer::new());
	Fs::new().register_endpoints(&mut rpc);
	Systemd.register_endpoints(&mut rpc);
	Pty::new(dialer.clone()).register_endpoints(&mut rpc);
	Subprocess::new(dialer.clone()).register_endpoints(&mut rpc);
	NixDaemon::new(dialer.clone()).register_endpoints(&mut rpc);
	IrohTunnel::new(dialer.clone()).register_endpoints(&mut rpc);
	Forward::new(dialer.clone()).register_endpoints(&mut rpc);

	remowt_plugin::host::serve(&mut rpc);

	let user_prompter = PromptEndpointsClient::wrap(rpc.remote(Address::User));
	let editor_client = EditorEndpointsClient::wrap(rpc.remote(Address::User));

	let bus = bus::spawn().await?;
	askpass::serve(&bus.conn, user_prompter.clone()).await?;
	editor::serve(&bus.conn, editor_client).await?;

	let helpers = tempfile::Builder::new().prefix("remowt-path.").tempdir()?;
	let exe = std::env::current_exe()?;
	let askpass_helper = helpers.path().join("remowt-askpass");
	let editor_helper = helpers.path().join("remowt-editor");
	let forward_helper = helpers.path().join("remowt-forward");
	{
		let script = format!(
			"#!/bin/sh\nexec {} ask-pass \"password\" \"$1\"\n",
			sh_quote(&exe.to_string_lossy())
		);
		fs::write(&askpass_helper, script).await?;
		fs::set_permissions(&askpass_helper, Permissions::from_mode(0o755)).await?;
	}
	{
		let script = format!(
			"#!/bin/sh\nexec {} editor \"$1\"\n",
			sh_quote(&exe.to_string_lossy())
		);
		fs::write(&editor_helper, script).await?;
		fs::set_permissions(&editor_helper, Permissions::from_mode(0o755)).await?;
	}
	{
		let script = format!(
			"#!/bin/sh\nexec {} forward \"$@\"\n",
			sh_quote(&exe.to_string_lossy())
		);
		fs::write(&forward_helper, script).await?;
		fs::set_permissions(&forward_helper, Permissions::from_mode(0o755)).await?;
	}

	// Safety: Hoping tokio own threads won't read any of those...
	unsafe {
		prepend_path(helpers.path());
		std::env::set_var("SUDO_ASKPASS", &askpass_helper);
		std::env::set_var("SSH_ASKPASS", &askpass_helper);
		std::env::set_var("SSH_ASKPASS_REQUIRE", "force");
		std::env::set_var("EDITOR", &editor_helper);
		std::env::set_var("VISUAL", &editor_helper);
		std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &bus.address);
	}

	let port = match path {
		Some(path) => from_socket(UnixStream::connect(path).await?),
		None => from_stdio(),
	};
	rpc.add_direct(Address::User, port, bifrostlink::Rtt(0));

	let polkit_conn = if !privileged && !local {
		// The unprivileged agent doubles as a polkit authentication agent so
		// `run0` (e.g. our own elevation) routes its prompt to the User over
		// bifrost instead of failing on a tty-less session.
		let conn = Connection::system().await?;
		let helper = SocketHelper {
			fallback: SuidHelper,
		};
		register_auth_agent(&conn, Agent::new(helper, user_prompter)).await?;
		Some(conn)
	} else {
		None
	};

	let _keep_alive = (bus, helpers, polkit_conn);
	pending().await
}

async fn register_auth_agent<H, P>(conn: &Connection, agent: Agent<H, P>) -> anyhow::Result<()>
where
	H: Helper + Clone + Send + Sync + 'static,
	P: Prompter + Clone + Send + Sync + 'static,
{
	let proxy = zbus_polkit::policykit1::AuthorityProxy::new(conn).await?;
	conn.object_server().at(OBJ_PATH, agent).await?;

	let subject = auth_agent_subject()?;
	proxy
		.register_authentication_agent(&subject, "C", OBJ_PATH)
		.await?;
	debug!(kind = subject.subject_kind, "registered polkit agent");
	Ok(())
}

fn auth_agent_subject() -> anyhow::Result<Subject> {
	let mut details = HashMap::new();
	if let Ok(session_id) = std::env::var("XDG_SESSION_ID") {
		let val: OwnedValue = Str::from(session_id).into();
		details.insert("session-id".to_string(), val);
		return Ok(Subject {
			subject_kind: "unix-session".to_string(),
			subject_details: details,
		});
	}

	details.insert("pid".to_string(), OwnedValue::from(std::process::id()));
	Ok(Subject {
		subject_kind: "unix-process".to_string(),
		subject_details: details,
	})
}

fn sh_quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "'\\''"))
}

/// Prepend `dir` to the process `PATH`.
///
/// # SAFETY
///
/// Same as `set_var`
unsafe fn prepend_path(dir: &std::path::Path) {
	let value = match std::env::var_os("PATH") {
		Some(existing) => {
			let mut v = dir.as_os_str().to_owned();
			v.push(":");
			v.push(existing);
			v
		}
		None => dir.as_os_str().to_owned(),
	};
	unsafe {
		std::env::set_var("PATH", value);
	}
}
