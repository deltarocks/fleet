use std::process::Stdio;
use std::sync::Arc;

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use remowt_link_shared::iroh_tunnel::{TunnelAddr, TunnelDialer};
use serde::{Deserialize, Serialize};
use std::result::Result;
use tokio::process::Command;

#[derive(Clone)]
pub struct NixDaemon {
	dialer: Arc<TunnelDialer>,
}

impl NixDaemon {
	pub fn new(dialer: Arc<TunnelDialer>) -> Self {
		Self { dialer }
	}
}

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("nix daemon unavailable: {0}")]
	DaemonUnavailable(String),
	#[error("tunnel socket unavailable: {0}")]
	Tunnel(String),
}

#[endpoints(ns = 4)]
impl NixDaemon {
	#[endpoints(id = 2)]
	async fn serve_store(&self, store: String, tunnel: TunnelAddr) -> Result<(), Error> {
		let mut child = Command::new("nix-daemon")
			.arg("--stdio")
			.arg("--store")
			.arg(&store)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.spawn()
			.map_err(|e| Error::DaemonUnavailable(e.to_string()))?;
		let tunnel = self
			.dialer
			.connect_tunnel(&tunnel)
			.await
			.map_err(|e| Error::Tunnel(e.to_string()))?;
		let mut stdin = child.stdin.take().expect("piped");
		let mut stdout = child.stdout.take().expect("piped");
		tokio::spawn(async move {
			let (mut tr, mut tw) = tokio::io::split(tunnel);
			let _ = tokio::join!(
				tokio::io::copy(&mut tr, &mut stdin),
				tokio::io::copy(&mut stdout, &mut tw),
			);
			let _ = child.wait().await;
		});
		Ok(())
	}
}
