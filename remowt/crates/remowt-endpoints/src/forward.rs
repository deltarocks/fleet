use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use remowt_link_shared::iroh_tunnel::{TunnelAddr, TunnelDialer};
use serde::{Deserialize, Serialize};
use std::result::Result;
use tokio::net::{TcpStream, UdpSocket};
use tracing::warn;

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("tunnel unavailable: {0}")]
	Tunnel(String),
	#[error("connect to {0} failed: {1}")]
	Connect(String, String),
	#[error("invalid target address {0:?}")]
	BadAddr(String),
	#[error("udp forward requires the iroh fast path, which is not established")]
	NoIroh,
}

#[derive(Clone)]
pub struct Forward {
	dialer: Arc<TunnelDialer>,
	next_session: Arc<AtomicU64>,
}

impl Forward {
	pub fn new(dialer: Arc<TunnelDialer>) -> Self {
		Self {
			dialer,
			next_session: Arc::new(AtomicU64::new(0)),
		}
	}
}

#[endpoints(ns = 12)]
impl Forward {
	#[endpoints(id = 1)]
	async fn connect_tcp(&self, tunnel: TunnelAddr, addr: String) -> Result<(), Error> {
		let stream = self
			.dialer
			.connect_tunnel(&tunnel)
			.await
			.map_err(|e| Error::Tunnel(e.to_string()))?;
		let tcp = TcpStream::connect(&addr)
			.await
			.map_err(|e| Error::Connect(addr, e.to_string()))?;
		tokio::spawn(async move {
			let mut stream = stream;
			let mut tcp = tcp;
			let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
		});
		Ok(())
	}

	#[endpoints(id = 2)]
	async fn open_udp(&self, addr: String) -> Result<u64, Error> {
		let target: SocketAddr = addr.parse().map_err(|_| Error::BadAddr(addr.clone()))?;
		let router = self.dialer.router().ok_or(Error::NoIroh)?;
		let session = self.next_session.fetch_add(1, Ordering::Relaxed);
		let mut rx = router.register(session);

		let sockets: Arc<Mutex<HashMap<u64, Arc<UdpSocket>>>> =
			Arc::new(Mutex::new(HashMap::new()));
		tokio::spawn(async move {
			while let Some((sub, payload)) = rx.recv().await {
				let existing = sockets.lock().expect("lock").get(&sub).cloned();
				let sock = match existing {
					Some(s) => s,
					None => {
						let sock = match UdpSocket::bind(unspecified_for(&target)).await {
							Ok(s) => s,
							Err(e) => {
								warn!("udp forward: bind failed: {e}");
								continue;
							}
						};
						if let Err(e) = sock.connect(target).await {
							warn!("udp forward: connect {target} failed: {e}");
							continue;
						}
						let sock = Arc::new(sock);
						sockets.lock().expect("lock").insert(sub, sock.clone());
						// Reply reader: datagrams from the target go back on (session, sub).
						let router = router.clone();
						let reply_sock = sock.clone();
						tokio::spawn(async move {
							let mut buf = vec![0u8; 65535];
							while let Ok(n) = reply_sock.recv(&mut buf).await {
								if router.send(session, sub, &buf[..n]).is_err() {
									break;
								}
							}
						});
						sock
					}
				};
				let _ = sock.send(&payload).await;
			}
			router.unregister(session);
		});

		Ok(session)
	}
}

fn unspecified_for(target: &SocketAddr) -> SocketAddr {
	match target {
		SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
		SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
	}
}
