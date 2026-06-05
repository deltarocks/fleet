use std::path::PathBuf;

use anyhow::{Context as _, Result};
use bifrostlink::Config;
use bifrostlink::declarative::endpoints;
use camino::Utf8PathBuf;
use remowt_client::Remowt;
use remowt_endpoints::nix_daemon::NixDaemonClient;
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tracing::error;

pub struct Nix;
pub use nix_eval::{init_libraries, init_tokio_for_nix};

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum NixError {
	#[error("nix daemon unavailable: {0}")]
	DaemonUnavailable(String),
	#[error("tunnel socket unavailable: {0}")]
	Tunnel(String),
	#[error("profile switch failed: {0}")]
	Profile(String),
	#[error("signing failed: {0}")]
	Sign(String),
	#[error("listing generations failed: {0}")]
	ListGenerations(String),
}

#[endpoints(ns = 91)]
impl Nix {
	#[endpoints(id = 3)]
	async fn switch_profile(
		&self,
		profile: String,
		store_path: Utf8PathBuf,
	) -> Result<(), NixError> {
		tokio::task::spawn_blocking(move || nix_eval::switch_profile(&profile, &store_path))
			.await
			.map_err(|e| NixError::Profile(e.to_string()))?
			.map_err(|e| NixError::Profile(e.to_string()))
	}

	#[endpoints(id = 4)]
	async fn sign_closure(
		&self,
		store_path: Utf8PathBuf,
		key_file: Utf8PathBuf,
	) -> Result<(), NixError> {
		tokio::task::spawn_blocking(move || {
			nix_eval::sign_closure(store_path.as_str(), key_file.as_str())
		})
		.await
		.map_err(|e| NixError::Sign(e.to_string()))?
		.map_err(|e| NixError::Sign(e.to_string()))
	}

	#[endpoints(id = 5)]
	async fn list_generations(
		&self,
		profile: String,
	) -> Result<Vec<nix_eval::ProfileGeneration>, NixError> {
		tokio::task::spawn_blocking(move || {
			nix_eval::list_generations(&format!("/nix/var/nix/profiles/{profile}"))
		})
		.await
		.map_err(|e| NixError::ListGenerations(e.to_string()))?
		.map_err(|e| NixError::ListGenerations(e.to_string()))
	}
}

pub async fn nix_store_socket(conn: Remowt, store: &str) -> Result<PathBuf> {
	let store = store.to_owned();
	let path = std::env::temp_dir().join(format!("fleet-nix-{}.sock", uuid::Uuid::new_v4()));
	let _ = std::fs::remove_file(&path);
	let listener = UnixListener::bind(&path)?;
	tokio::spawn(async move {
		if let Err(e) = serve(conn, listener, store).await {
			error!("nix daemon proxy failed: {e}");
		}
	});
	Ok(path)
}

async fn serve(conn: Remowt, listener: UnixListener, store: String) -> Result<()> {
	let nix = conn.endpoints::<NixDaemonClient<_>>();
	loop {
		let (mut local, _) = listener.accept().await?;

		let (rx, remote_sock) = match conn.bind_runtime_unix("nix-daemon").await {
			Ok(rx) => rx,
			Err(e) => {
				error!("streamlocal_forward failed: {e}");
				continue;
			}
		};
		let sock_str = remote_sock.as_str().to_owned();
		match nix.serve_store(store.clone(), sock_str).await {
			Ok(Ok(())) => {}
			Ok(Err(e)) => {
				error!("nix bridge: {e}");
				continue;
			}
			Err(e) => {
				error!("nix bridge rpc failed: {e}");
				continue;
			}
		}

		let mut channel = rx
			.accept()
			.await
			.context("failed to accept remote nix connection")?;
		tokio::spawn(async move {
			if let Err(e) = tokio::io::copy_bidirectional(&mut local, &mut channel).await {
				tracing::debug!("nix tunnel ended: {e}");
			}
		});
	}
}
