use std::env::{current_dir, temp_dir};
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use anyhow::{anyhow, bail, Context as _};
use bifrostlink::declarative::RemoteEndpoints as _;
use nix::libc;
use remowt_link_shared::editor::EditorEndpointsClient;
use remowt_link_shared::{gateway, Address, BifConfig};
use tokio::process::Command;

pub async fn forward(udp: bool, addr: String) -> anyhow::Result<()> {
	let rpc = gateway::connect(&gateway::socket_path()?).await?;
	let editor = EditorEndpointsClient::<BifConfig>::wrap(rpc.remote(Address::User));
	let local = if udp {
		editor.expose_udp(addr).await
	} else {
		editor.expose_tcp(addr).await
	}
	.map_err(|e| anyhow!("requesting forward on the User: {e}"))?
	.map_err(|e| anyhow!("forward failed: {e}"))?;
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

	let rpc = gateway::connect(&gateway::socket_path()?).await?;
	let editor = EditorEndpointsClient::<BifConfig>::wrap(rpc.remote(Address::User));
	let result = editor
		.open_editor(sock_str)
		.await
		.map_err(|e| anyhow!("requesting editor on the User: {e}"))
		.and_then(|r| r.map_err(|e| anyhow!("editor failed: {e}")));

	if tokio::time::timeout(Duration::from_secs(2), child.wait())
		.await
		.is_err()
	{
		let _ = child.kill().await;
	}
	let _ = fs::remove_file(&sock);

	result
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
