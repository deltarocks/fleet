use std::sync::{Arc, Mutex};

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use camino::Utf8PathBuf;
use iroh::{Endpoint, EndpointId, SecretKey};
use remowt_link_shared::iroh_tunnel::{build_endpoint, TunnelDialer};
use serde::{Deserialize, Serialize};
use std::result::Result;
use tokio::net::UnixStream;
use tracing::{debug, warn};

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("iroh transport pipe unavailable: {0}")]
	Pipe(String),
	#[error("iroh endpoint failed: {0}")]
	Iroh(String),
}

#[derive(Clone)]
pub struct IrohTunnel {
	dialer: Arc<TunnelDialer>,
	endpoint: Arc<Mutex<Option<Endpoint>>>,
}

impl IrohTunnel {
	pub fn new(dialer: Arc<TunnelDialer>) -> Self {
		Self {
			dialer,
			endpoint: Arc::new(Mutex::new(None)),
		}
	}
}

#[endpoints(ns = 11)]
impl IrohTunnel {
	#[endpoints(id = 1)]
	async fn setup(
		&self,
		client_id: EndpointId,
		xport_socket: Utf8PathBuf,
	) -> Result<EndpointId, Error> {
		let stream = UnixStream::connect(&xport_socket)
			.await
			.map_err(|e| Error::Pipe(e.to_string()))?;

		let secret = SecretKey::generate();
		let agent_id = secret.public();
		let ep = build_endpoint(secret, stream, client_id, true)
			.await
			.map_err(|e| Error::Iroh(e.to_string()))?;

		let dialer = self.dialer.clone();
		let accept_ep = ep.clone();
		tokio::spawn(async move {
			while let Some(incoming) = accept_ep.accept().await {
				let dialer = dialer.clone();
				match incoming.accept() {
					Ok(accepting) => {
						tokio::spawn(async move {
							match accepting.await {
								Ok(conn) => {
									if conn.remote_id() != client_id {
										warn!("iroh: rejecting connection from unexpected peer");
										conn.close(0u32.into(), b"unexpected peer");
										return;
									}
									debug!("iroh tunnel connection accepted");
									dialer.set_conn(conn);
								}
								Err(e) => warn!("iroh accept failed: {e}"),
							}
						});
					}
					Err(e) => warn!("iroh incoming rejected: {e}"),
				}
			}
		});

		*self.endpoint.lock().expect("not poisoned") = Some(ep);

		Ok(agent_id)
	}
}
