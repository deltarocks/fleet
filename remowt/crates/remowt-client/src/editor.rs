use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use remowt_endpoints::forward::ForwardClient;
use remowt_link_shared::editor::{EditorBackend, Error};
use remowt_link_shared::BifConfig;
use russh::client::Handle;
use tokio::net::{TcpListener, UdpSocket, UnixListener};
use tracing::error;

use crate::{Remowt, SshHandler};

pub struct SshEditor {
	pub sess: Arc<Handle<SshHandler>>,
	pub conn: Remowt,
}
impl EditorBackend for SshEditor {
	async fn open_editor(&self, socket_path: String) -> Result<(), Error> {
		let local = std::env::temp_dir().join(format!("remowt-nvim-{}.sock", uuid::Uuid::new_v4()));
		let _ = std::fs::remove_file(&local);
		let listener = UnixListener::bind(&local).map_err(|e| Error::Failed(e.to_string()))?;

		let sess = self.sess.clone();
		let forward = tokio::spawn(async move {
			loop {
				let Ok((mut stream, _)) = listener.accept().await else {
					break;
				};
				let sess = sess.clone();
				let remote = socket_path.clone();
				tokio::spawn(async move {
					match sess.channel_open_direct_streamlocal(remote).await {
						Ok(ch) => {
							let mut remote = ch.into_stream();
							let _ = tokio::io::copy_bidirectional(&mut stream, &mut remote).await;
						}
						Err(e) => error!("opening direct-streamlocal to nvim failed: {e}"),
					}
				});
			}
		});

		let status = tokio::process::Command::new("neovide")
			.arg("--no-fork")
			.arg("--server")
			.arg(&local)
			.status()
			.await
			.map_err(|e| Error::Failed(format!("spawning neovide: {e}")));

		forward.abort();
		let _ = std::fs::remove_file(&local);

		match status? {
			s if s.success() => Ok(()),
			s => Err(Error::Failed(format!("neovide exited with {s}"))),
		}
	}

	async fn expose_tcp(&self, addr: String) -> Result<u16, Error> {
		let listener = TcpListener::bind(("127.0.0.1", 0))
			.await
			.map_err(|e| Error::Failed(e.to_string()))?;
		let local = listener
			.local_addr()
			.map_err(|e| Error::Failed(e.to_string()))?
			.port();

		let conn = self.conn.clone();
		tokio::spawn(async move {
			loop {
				let Ok((mut tcp, _)) = listener.accept().await else {
					break;
				};
				let conn = conn.clone();
				let addr = addr.clone();
				tokio::spawn(async move {
					let (forwarded, tunnel) = match conn.bind_fast_tunnel("forward", false).await {
						Ok(v) => v,
						Err(e) => {
							error!("forward: bind tunnel failed: {e}");
							return;
						}
					};
					let fclient: ForwardClient<BifConfig> = conn.endpoints();
					match fclient.connect_tcp(tunnel, addr).await {
						Ok(Ok(())) => {}
						Ok(Err(e)) => {
							error!("forward: agent connect_tcp failed: {e}");
							return;
						}
						Err(e) => {
							error!("forward: connect_tcp rpc failed: {e}");
							return;
						}
					}
					match forwarded.accept().await {
						Ok(mut stream) => {
							let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
						}
						Err(e) => error!("forward: accept tunnel failed: {e}"),
					}
				});
			}
		});

		Ok(local)
	}

	async fn expose_udp(&self, addr: String) -> Result<u16, Error> {
		let router = self.conn.datagram_router().ok_or_else(|| {
			Error::Failed(
				"udp forward requires the iroh fast path, which is not established".into(),
			)
		})?;

		let fclient: ForwardClient<BifConfig> = self.conn.endpoints();
		let session = fclient
			.open_udp(addr)
			.await
			.map_err(|e| Error::Failed(format!("open_udp rpc: {e}")))?
			.map_err(|e| Error::Failed(format!("agent open_udp: {e}")))?;

		let sock = Arc::new(
			UdpSocket::bind(("127.0.0.1", 0))
				.await
				.map_err(|e| Error::Failed(e.to_string()))?,
		);
		let local = sock
			.local_addr()
			.map_err(|e| Error::Failed(e.to_string()))?
			.port();

		let sub_for_source: Arc<Mutex<HashMap<SocketAddr, u64>>> =
			Arc::new(Mutex::new(HashMap::new()));
		let source_for_sub: Arc<Mutex<HashMap<u64, SocketAddr>>> =
			Arc::new(Mutex::new(HashMap::new()));
		let next_sub = Arc::new(AtomicU64::new(0));
		let mut rx = router.register(session);

		let up_sock = sock.clone();
		let up_router = router.clone();
		let down_source_for_sub = source_for_sub.clone();
		tokio::spawn(async move {
			let mut buf = vec![0u8; 65535];
			loop {
				let (n, src) = match up_sock.recv_from(&mut buf).await {
					Ok(v) => v,
					Err(_) => break,
				};
				let sub = {
					let mut by_src = sub_for_source.lock().expect("lock");
					if let Some(&sub) = by_src.get(&src) {
						sub
					} else {
						let sub = next_sub.fetch_add(1, Ordering::Relaxed);
						by_src.insert(src, sub);
						source_for_sub.lock().expect("lock").insert(sub, src);
						sub
					}
				};
				if up_router.send(session, sub, &buf[..n]).is_err() {
					break;
				}
			}
			up_router.unregister(session);
		});

		let down_sock = sock.clone();
		tokio::spawn(async move {
			while let Some((sub, payload)) = rx.recv().await {
				let dst = down_source_for_sub.lock().expect("lock").get(&sub).copied();
				if let Some(dst) = dst {
					let _ = down_sock.send_to(&payload, dst).await;
				}
			}
		});

		Ok(local)
	}
}
