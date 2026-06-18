use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _};
use bifrostlink::{Rpc, Rtt};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::port::child_port;
use crate::{Address, BifConfig};

pub const SOCKET_ENV: &str = "REMOWT_AGENT_SOCKET";

pub const SOCKET_NAME: &str = "agent.sock";

pub fn local_socket() -> anyhow::Result<PathBuf> {
	let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
	Ok(PathBuf::from(dir).join("remowt-local"))
}

pub fn socket_path() -> anyhow::Result<PathBuf> {
	match std::env::var_os(SOCKET_ENV) {
		Some(p) => Ok(PathBuf::from(p)),
		None => local_socket(),
	}
}

fn peer_tag(addr: &Address) -> u8 {
	match addr {
		Address::User => 0,
		Address::Agent => 1,
		Address::AgentPrivileged => 2,
		_ => unreachable!(),
	}
}
fn untag_peer(tag: u8) -> anyhow::Result<Address> {
	Ok(match tag {
		0 => Address::User,
		1 => Address::Agent,
		2 => Address::AgentPrivileged,
		_ => unreachable!(),
	})
}

pub async fn serve(rpc: Rpc<BifConfig>, path: &Path) -> anyhow::Result<()> {
	let _ = tokio::fs::remove_file(path).await;
	let listener = UnixListener::bind(path)
		.with_context(|| format!("binding agent gateway at {}", path.display()))?;
	let tag = peer_tag(&rpc.me());
	tokio::spawn(async move {
		loop {
			let mut stream = match listener.accept().await {
				Ok((stream, _)) => stream,
				Err(e) => {
					warn!("gateway accept failed: {e}");
					continue;
				}
			};
			let id = Uuid::new_v4().as_u128();
			let mut hello = [0u8; 17];
			hello[0] = tag;
			hello[1..].copy_from_slice(&id.to_be_bytes());
			if let Err(e) = stream.write_all(&hello).await {
				warn!("gateway handshake failed: {e}");
				continue;
			}
			debug!("gateway client {id:032x}");
			let (rx, tx) = stream.into_split();
			rpc.add_direct(
				Address::Ephemeral(Uuid::from_u128(id)),
				child_port(rx, tx),
				Rtt(0),
			);
		}
	});
	Ok(())
}

pub async fn connect(path: &Path) -> anyhow::Result<Rpc<BifConfig>> {
	let mut stream = UnixStream::connect(path)
		.await
		.with_context(|| format!("connecting to agent gateway at {}", path.display()))?;

	let mut hello = [0u8; 17];
	stream
		.read_exact(&mut hello)
		.await
		.context("reading gateway handshake")?;
	let peer = untag_peer(hello[0])?;
	let id = u128::from_be_bytes(hello[1..].try_into().expect("16 bytes"));

	let (rx, tx) = stream.into_split();
	let rpc = Rpc::<BifConfig>::new(Address::Ephemeral(Uuid::from_u128(id)));
	rpc.add_direct(peer, child_port(rx, tx), Rtt(0));
	rpc.wait_for_connection_to(Address::User)
		.await
		.map_err(|_| anyhow!("no route to the User through the agent"))?;
	Ok(rpc)
}
