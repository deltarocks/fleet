use std::env::{current_dir, temp_dir};
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use anyhow::{bail, Context as _};
use nix::libc;
use remowt_link_shared::editor::EditorEndpointsClient;
use tokio::process::Command;
use zbus::{fdo, interface, proxy, Connection};

use remowt_link_shared::BifConfig;

const BUS_NAME: &str = "lach.RemowtEditor";
const SERVICE_PATH: &str = "/lach/Editor";

pub struct EditorService {
	editor: EditorEndpointsClient<BifConfig>,
}

#[interface(name = "lach.RemowtEditor")]
impl EditorService {
	/// Attach the User's GUI to the nvim server at `socket_path` (on the remote),
	/// blocking until the user is done.
	async fn edit(&self, socket_path: String) -> fdo::Result<()> {
		self.editor
			.open_editor(socket_path)
			.await
			.map_err(|e| fdo::Error::Failed(format!("requesting editor on the User: {e}")))?
			.map_err(|e| fdo::Error::Failed(format!("editor failed: {e}")))?;
		Ok(())
	}

	async fn forward_tcp(&self, addr: String) -> fdo::Result<u16> {
		let local = self
			.editor
			.expose_tcp(addr)
			.await
			.map_err(|e| fdo::Error::Failed(format!("requesting tcp forward on the User: {e}")))?
			.map_err(|e| fdo::Error::Failed(format!("tcp forward failed: {e}")))?;
		Ok(local)
	}

	async fn forward_udp(&self, addr: String) -> fdo::Result<u16> {
		let local = self
			.editor
			.expose_udp(addr)
			.await
			.map_err(|e| fdo::Error::Failed(format!("requesting udp forward on the User: {e}")))?
			.map_err(|e| fdo::Error::Failed(format!("udp forward failed: {e}")))?;
		Ok(local)
	}
}

pub async fn serve(
	conn: &Connection,
	editor: EditorEndpointsClient<BifConfig>,
) -> anyhow::Result<()> {
	conn.object_server()
		.at(SERVICE_PATH, EditorService { editor })
		.await?;
	conn.request_name(BUS_NAME).await?;
	Ok(())
}

#[proxy(interface = "lach.RemowtEditor")]
trait RemowtEditor {
	async fn edit(&self, socket_path: &str) -> fdo::Result<()>;
	async fn forward_tcp(&self, addr: &str) -> fdo::Result<u16>;
	async fn forward_udp(&self, addr: &str) -> fdo::Result<u16>;
}

pub async fn forward(udp: bool, addr: String) -> anyhow::Result<()> {
	let conn = Connection::session()
		.await
		.context("connecting to the session bus (DBUS_SESSION_BUS_ADDRESS)")?;
	let proxy = RemowtEditorProxy::builder(&conn)
		.destination(BUS_NAME)?
		.path(SERVICE_PATH)?
		.build()
		.await?;
	let local = if udp {
		proxy.forward_udp(&addr).await?
	} else {
		proxy.forward_tcp(&addr).await?
	};
	println!("{local}");
	Ok(())
}

pub async fn edit(path: String) -> anyhow::Result<()> {
	let path = Path::new(&path);
	let abs = if path.is_absolute() {
		path.to_path_buf()
	} else {
		current_dir()?.join(path)
	};

	let sock = temp_dir().join(format!("remowt-nvim-{}.sock", uuid::Uuid::new_v4()));
	let sock_str = sock
		.to_str()
		.context("temp socket path is not utf-8")?
		.to_owned();

	let mut child = Command::new("nvim");
	child
		.arg("--headless")
		.arg("--listen")
		.arg(&sock)
		.arg("--")
		.arg(&abs)
		.kill_on_drop(true);
	// SAFETY: only an async-signal-safe `prctl` call.
	unsafe {
		child.pre_exec(|| {
			if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(())
		});
	}
	let mut child = child.spawn().context("spawning nvim")?;

	wait_for_socket(&sock)
		.await
		.context("nvim did not start its server")?;

	let conn = Connection::session()
		.await
		.context("connecting to the session bus (DBUS_SESSION_BUS_ADDRESS)")?;
	let proxy = RemowtEditorProxy::builder(&conn)
		.destination(BUS_NAME)?
		.path(SERVICE_PATH)?
		.build()
		.await?;
	let result = proxy.edit(&sock_str).await;

	if tokio::time::timeout(Duration::from_secs(2), child.wait())
		.await
		.is_err()
	{
		let _ = child.kill().await;
	}
	let _ = fs::remove_file(&sock);

	result?;
	Ok(())
}

async fn wait_for_socket(path: &Path) -> anyhow::Result<()> {
	for _ in 0..200 {
		if tokio::fs::try_exists(path).await.unwrap_or(false) {
			return Ok(());
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	bail!("timed out waiting for {}", path.display())
}
