use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::future::pending;
use std::sync::LazyLock;

use anyhow::Context as _;
use clap::Parser;
use nix::unistd::{setuid, Uid, User};
use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use remowt_polkit_shared::BackendRequest;
use remowt_ui_prompt::dbus::DbusPrompterProxyBlocking;
use remowt_ui_prompt::BlockingPrompter;
use tokio::task::{block_in_place, spawn_blocking};
use tracing::trace;
use zbus::fdo;
use zbus::message::Header;
use zbus::zvariant::OwnedValue;
use zbus::{blocking, interface, proxy, Connection};

struct Helper {
	connection: Connection,
	blocking_connection: blocking::Connection,
}

static ALLOWED_ENVIRONMENT: LazyLock<HashSet<&str>> = LazyLock::new(|| {
	[
		// pam ssh agent auth
		"SSH_AUTH_SOCK",
		// ssh itself provides this when running PAM
		"SSH_AUTH_INFO_0",
		// contains user which ran sudo
		"SUDO_USER",
	]
	.into_iter()
	.collect()
});

struct Conversation<P>(P);
impl<P: BlockingPrompter> Conversation<P> {
	fn prompt_inner(&self, echo: bool, prompt: &CStr) -> Result<CString, ErrorCode> {
		trace!("do prompt");
		let out = self
			.0
			.prompt_text(echo, &prompt.to_string_lossy(), "PAM prompt request", &[])
			.map_err(|e| {
				trace!("prompt error: {e}");
				ErrorCode::CONV_ERR
			})?;
		CString::new(out).map_err(|_| ErrorCode::CONV_AGAIN)
	}
	fn text_inner(&self, error: bool, msg: &CStr) {
		trace!("do text");
		let msg = msg.to_string_lossy();
		let _ = self.0.display_text(error, &msg, &[]);
	}
}
impl<P: BlockingPrompter> ConversationHandler for Conversation<P> {
	fn prompt_echo_on(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
		self.prompt_inner(true, prompt)
	}

	fn prompt_echo_off(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
		self.prompt_inner(false, prompt)
	}

	fn text_info(&mut self, msg: &CStr) {
		self.text_inner(false, msg)
	}

	fn error_msg(&mut self, msg: &CStr) {
		self.text_inner(true, msg)
	}

	fn radio_prompt(&mut self, prompt: &CStr) -> Result<bool, ErrorCode> {
		let prompt = prompt.to_string_lossy();
		let result = self
			.0
			.prompt_radio(&prompt, "PAM prompt request", &[])
			.map_err(|_| ErrorCode::CONV_ERR)?;
		Ok(result)
	}
}

#[proxy(
	default_service = "org.freedesktop.DBus",
	default_path = "/org/freedesktop/DBus"
)]
trait DBus {
	fn get_connection_credentials(&self, body: &str) -> zbus::Result<HashMap<String, OwnedValue>>;
}

#[interface(name = "lach.PolkitHelper")]
impl Helper {
	async fn init_conversation(
		&self,
		request: BackendRequest,
		#[zbus(header)] hdr: Header<'_>,
	) -> fdo::Result<()> {
		let Some(sender) = hdr.sender().map(|v| v.to_owned()) else {
			trace!("missing sender");
			return Err(fdo::Error::AuthFailed("missing sender".to_owned()));
		};

		let dbus = DBusProxy::new(&self.connection).await?;

		// TOCTOU: sender might be already disconnected, and there might be another
		// user with different user id here, but does it matters?
		let reply = dbus.get_connection_credentials(&sender).await?;
		let connection_uid: u32 = (&reply["UnixUserID"]).try_into().unwrap();

		let identity = request.identity.clone();
		let blocking_connection = self.blocking_connection.clone();
		let thread_result: fdo::Result<()> = block_in_place(move || {
			trace!("find user");
			let Some(identity_uid) = identity.uid() else {
				return Err(fdo::Error::AuthFailed("can't process identity".to_owned()));
			};
			let user = User::from_uid(identity_uid)
				.map_err(|_| fdo::Error::AuthFailed("error querying user".to_owned()))?
				.ok_or_else(|| fdo::Error::AuthFailed("uid not found".to_owned()))?;

			let responder = DbusPrompterProxyBlocking::new(
				&blocking_connection,
				sender,
				request.prompter_path,
			)?;
			let conversation = Conversation(responder);
			trace!("run context for {}", &user.name);
			let mut ctx = Context::new(
				// TODO: Should another scope be used?
				"login",
				Some(&user.name),
				conversation,
			)
			.map_err(|_| fdo::Error::Failed("pam context init failed".to_owned()))?;

			trace!("fill env");
			for (k, v) in request.environment {
				if k.contains('=') || !ALLOWED_ENVIRONMENT.contains(k.as_str()) {
					continue;
				}
				let _ = ctx.putenv(format!("{k}={v}"));
			}

			trace!("authenticate");
			ctx.authenticate(Flag::NONE)
				.map_err(|_| fdo::Error::AuthFailed("pam authentication failed".to_owned()))?;

			trace!("acct mgmt");
			ctx.acct_mgmt(Flag::NONE)
				.map_err(|_| fdo::Error::AuthFailed("pam acct mgmt failed".to_owned()))?;

			Ok(())
		});

		thread_result?;

		trace!("respond");
		let proxy = zbus_polkit::policykit1::AuthorityProxy::new(&self.connection).await?;

		let identity_details = request
			.identity
			.details
			.iter()
			.map(|(k, v)| (k.as_str(), (**v).try_clone().expect("success")))
			.collect::<HashMap<_, _>>();
		proxy
			.authentication_agent_response2(
				connection_uid,
				&request.cookie,
				&zbus_polkit::policykit1::Identity {
					identity_kind: &request.identity.kind,
					identity_details: &identity_details,
				},
			)
			.await?;
		Ok(())
	}
}

const OBJ_PATH: &str = "/lach/PolkitHelper";

#[derive(Parser)]
struct Opts {
	/// Not recommended: start as a session connection, then use escalation
	/// to respond to polkit requests.
	#[arg(long)]
	session: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt::init();
	let opts = Opts::parse();
	let connection = if opts.session {
		Connection::session().await
	} else {
		Connection::system().await
	}
	.context("failed to open connection")?;

	let session = opts.session;
	let blocking_connection: anyhow::Result<blocking::Connection> = spawn_blocking(move || {
		Ok(if session {
			blocking::Connection::session()?
		} else {
			blocking::Connection::system()?
		})
	})
	.await?;
	let blocking_connection = blocking_connection.context("failed to open blocking connection")?;

	if opts.session {
		setuid(Uid::from_raw(0))
			.context("polkit-backend needs to be suid if run in session mode")?;
	}

	connection
		.object_server()
		.at(
			OBJ_PATH,
			Helper {
				connection: connection.clone(),
				blocking_connection,
			},
		)
		.await
		.context("failed listen path")?;

	connection
		.request_name("lach.polkit.helper1")
		.await
		.context("failed to request name")?;

	pending().await
}
