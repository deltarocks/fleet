use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use std::{fmt, io};

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use camino::Utf8PathBuf;
use futures::{SinkExt as _, Stream};
use iroh::endpoint::transports::{
	CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit,
};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointId, SecretKey};
use iroh_base::CustomAddr;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, warn};

// "rmwtssh"
pub const REMOWT_SSH_TRANSPORT_ID: u64 = 0x_72_6d_77_74_73_73_68;

pub const REMOWT_ALPN: &[u8] = b"remowt/tunnel/0";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TunnelAddr {
	Unix(Utf8PathBuf),
	Iroh { token: u64 },
}

pub fn ssh_custom_addr(id: EndpointId) -> CustomAddr {
	CustomAddr::from((REMOWT_SSH_TRANSPORT_ID, &id.as_bytes()[..]))
}

pub async fn build_endpoint<S>(
	secret: SecretKey,
	stream: S,
	remote: EndpointId,
	accept: bool,
) -> Result<Endpoint>
where
	S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
	let local = secret.public();
	let transport = Arc::new(SshTransport::new(stream, local, remote));
	let mut builder = Endpoint::builder(presets::N0)
		.secret_key(secret)
		.add_custom_transport(transport);
	if accept {
		builder = builder.alpns(vec![REMOWT_ALPN.to_vec()]);
	}
	Ok(builder.bind().await?)
}

struct SshTransport {
	local: CustomAddr,
	remote: CustomAddr,
	addrs: n0_watcher::Watchable<Vec<CustomAddr>>,
	endpoint: Mutex<Option<SshEndpointParts>>,
	sender: Arc<SshSender>,
}

struct SshEndpointParts {
	reader: Pin<Box<dyn FramedDatagrams>>,
}

trait FramedDatagrams: Stream<Item = io::Result<BytesMut>> + Send + Sync {}
impl<T: Stream<Item = io::Result<BytesMut>> + Send + Sync> FramedDatagrams for T {}

impl fmt::Debug for SshTransport {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SshTransport").finish_non_exhaustive()
	}
}

impl SshTransport {
	fn new<S>(stream: S, local: EndpointId, remote: EndpointId) -> Self
	where
		S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
	{
		let (read, write): (ReadHalf<S>, WriteHalf<S>) = tokio::io::split(stream);
		let reader = FramedRead::new(read, LengthDelimitedCodec::new());

		let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
		tokio::spawn(writer_task(rx, write));

		let local = ssh_custom_addr(local);
		Self {
			addrs: n0_watcher::Watchable::new(vec![local.clone()]),
			local,
			remote: ssh_custom_addr(remote),
			endpoint: Mutex::new(Some(SshEndpointParts {
				reader: Box::pin(reader),
			})),
			sender: Arc::new(SshSender { tx }),
		}
	}
}

async fn writer_task<W: AsyncWrite + Unpin>(mut rx: mpsc::UnboundedReceiver<Bytes>, write: W) {
	let mut framed = FramedWrite::new(write, LengthDelimitedCodec::new());
	while let Some(datagram) = rx.recv().await {
		if let Err(e) = framed.send(datagram).await {
			debug!("ssh transport writer ended: {e}");
			break;
		}
	}
}

impl CustomTransport for SshTransport {
	fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
		let parts = self
			.endpoint
			.lock()
			.expect("not poisoned")
			.take()
			.ok_or_else(|| io::Error::other("ssh transport already bound"))?;
		Ok(Box::new(SshEndpoint {
			local: self.local.clone(),
			remote: self.remote.clone(),
			addrs: self.addrs.clone(),
			sender: self.sender.clone(),
			reader: parts.reader,
		}))
	}
}

struct SshEndpoint {
	local: CustomAddr,
	remote: CustomAddr,
	addrs: n0_watcher::Watchable<Vec<CustomAddr>>,
	sender: Arc<SshSender>,
	reader: Pin<Box<dyn FramedDatagrams>>,
}

impl fmt::Debug for SshEndpoint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SshEndpoint").finish_non_exhaustive()
	}
}

impl CustomEndpoint for SshEndpoint {
	fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
		self.addrs.watch()
	}

	fn create_sender(&self) -> Arc<dyn CustomSender> {
		self.sender.clone()
	}

	fn poll_recv(
		&mut self,
		cx: &mut Context,
		bufs: &mut [io::IoSliceMut<'_>],
		metas: &mut [noq_udp::RecvMeta],
		recv_infos: &mut [RecvInfo],
	) -> Poll<io::Result<usize>> {
		assert_eq!(bufs.len(), metas.len());
		assert_eq!(bufs.len(), recv_infos.len());
		if bufs.is_empty() {
			return Poll::Ready(Ok(0));
		}
		let mut count = 0;
		while count < bufs.len() {
			match self.reader.as_mut().poll_next(cx) {
				Poll::Ready(Some(Ok(frame))) => {
					let buf = &mut bufs[count];
					if buf.len() < frame.len() {
						warn!("ssh transport datagram {} > buf {}", frame.len(), buf.len());
						if count > 0 {
							break;
						}
						return Poll::Ready(Err(io::Error::other("datagram exceeds recv buffer")));
					}
					buf[..frame.len()].copy_from_slice(&frame);
					metas[count].len = frame.len();
					metas[count].stride = frame.len();
					recv_infos[count] =
						RecvInfo::new(self.remote.clone(), Some(self.local.clone()));
					count += 1;
				}
				Poll::Ready(Some(Err(e))) => {
					if count > 0 {
						break;
					}
					return Poll::Ready(Err(e));
				}
				Poll::Ready(None) => {
					if count > 0 {
						break;
					}
					return Poll::Ready(Err(io::Error::other("ssh transport closed")));
				}
				Poll::Pending => {
					if count > 0 {
						break;
					}
					return Poll::Pending;
				}
			}
		}
		Poll::Ready(Ok(count))
	}
}

#[derive(Debug)]
struct SshSender {
	tx: mpsc::UnboundedSender<Bytes>,
}

impl CustomSender for SshSender {
	fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
		addr.id() == REMOWT_SSH_TRANSPORT_ID
	}

	fn poll_send(
		&self,
		_cx: &mut Context,
		_dst: &CustomAddr,
		_src: Option<&CustomAddr>,
		transmit: &Transmit<'_>,
	) -> Poll<io::Result<()>> {
		let segment = transmit.segment_size.unwrap_or(transmit.contents.len());
		if segment == 0 || transmit.contents.is_empty() {
			return Poll::Ready(Ok(()));
		}
		for chunk in transmit.contents.chunks(segment) {
			if self.tx.send(Bytes::copy_from_slice(chunk)).is_err() {
				return Poll::Ready(Err(io::Error::other("ssh transport writer gone")));
			}
		}
		Poll::Ready(Ok(()))
	}
}

pub struct IrohBiStream {
	send: SendStream,
	recv: RecvStream,
}

impl IrohBiStream {
	pub fn new(send: SendStream, recv: RecvStream) -> Self {
		Self { send, recv }
	}
}

impl AsyncRead for IrohBiStream {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		AsyncRead::poll_read(Pin::new(&mut self.get_mut().recv), cx, buf)
	}
}

impl AsyncWrite for IrohBiStream {
	fn poll_write(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		AsyncWrite::poll_write(Pin::new(&mut self.get_mut().send), cx, buf)
	}
	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		AsyncWrite::poll_flush(Pin::new(&mut self.get_mut().send), cx)
	}
	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		AsyncWrite::poll_shutdown(Pin::new(&mut self.get_mut().send), cx)
	}
}

pub enum TunnelStream {
	Unix(UnixStream),
	Iroh(IrohBiStream),
}

impl AsyncRead for TunnelStream {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		match self.get_mut() {
			TunnelStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
			TunnelStream::Iroh(s) => Pin::new(s).poll_read(cx, buf),
		}
	}
}

impl AsyncWrite for TunnelStream {
	fn poll_write(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		match self.get_mut() {
			TunnelStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
			TunnelStream::Iroh(s) => Pin::new(s).poll_write(cx, buf),
		}
	}
	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		match self.get_mut() {
			TunnelStream::Unix(s) => Pin::new(s).poll_flush(cx),
			TunnelStream::Iroh(s) => Pin::new(s).poll_flush(cx),
		}
	}
	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		match self.get_mut() {
			TunnelStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
			TunnelStream::Iroh(s) => Pin::new(s).poll_shutdown(cx),
		}
	}
}

#[derive(Clone)]
pub struct TunnelDialer {
	conn: watch::Sender<Option<Connection>>,
	router: Arc<Mutex<Option<Arc<DatagramRouter>>>>,
}

impl Default for TunnelDialer {
	fn default() -> Self {
		Self::new()
	}
}

impl TunnelDialer {
	pub fn new() -> Self {
		let (conn, _) = watch::channel(None);
		Self {
			conn,
			router: Arc::new(Mutex::new(None)),
		}
	}

	pub fn set_conn(&self, conn: Connection) {
		*self.router.lock().expect("lock") = Some(DatagramRouter::spawn(conn.clone()));
		self.conn.send_replace(Some(conn));
	}

	pub fn router(&self) -> Option<Arc<DatagramRouter>> {
		self.router.lock().expect("lock").clone()
	}

	pub async fn connect_tunnel(&self, addr: &TunnelAddr) -> io::Result<TunnelStream> {
		match addr {
			TunnelAddr::Unix(path) => Ok(TunnelStream::Unix(UnixStream::connect(path).await?)),
			TunnelAddr::Iroh { token } => {
				let mut rx = self.conn.subscribe();
				let conn =
					tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|c| c.is_some()))
						.await
						.map_err(|_| io::Error::other("timed out waiting for iroh connection"))?
						.map_err(|_| io::Error::other("iroh connection channel closed"))?
						.clone()
						.expect("is_some");
				let (mut send, recv) = conn.open_bi().await.map_err(io::Error::other)?;
				send.write_all(&token.to_be_bytes())
					.await
					.map_err(io::Error::other)?;
				Ok(TunnelStream::Iroh(IrohBiStream::new(send, recv)))
			}
		}
	}
}

const DGRAM_HEADER: usize = 16;

type DatagramRoutes = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<(u64, Bytes)>>>>;

pub struct DatagramRouter {
	conn: Connection,
	routes: DatagramRoutes,
}

impl DatagramRouter {
	pub fn spawn(conn: Connection) -> Arc<Self> {
		let routes: DatagramRoutes = Arc::new(Mutex::new(HashMap::new()));
		let read_routes = routes.clone();
		let read_conn = conn.clone();
		tokio::spawn(async move {
			loop {
				match read_conn.read_datagram().await {
					Ok(dg) if dg.len() >= DGRAM_HEADER => {
						let session = u64::from_be_bytes(dg[0..8].try_into().expect("8 bytes"));
						let sub = u64::from_be_bytes(dg[8..16].try_into().expect("8 bytes"));
						let payload = dg.slice(DGRAM_HEADER..);
						let tx = read_routes.lock().expect("lock").get(&session).cloned();
						if let Some(tx) = tx {
							let _ = tx.send((sub, payload));
						}
					}
					Ok(_) => {}
					Err(e) => {
						debug!("datagram read loop ended: {e}");
						break;
					}
				}
			}
		});
		Arc::new(Self { conn, routes })
	}

	pub fn register(&self, session: u64) -> mpsc::UnboundedReceiver<(u64, Bytes)> {
		let (tx, rx) = mpsc::unbounded_channel();
		self.routes.lock().expect("lock").insert(session, tx);
		rx
	}

	pub fn unregister(&self, session: u64) {
		self.routes.lock().expect("lock").remove(&session);
	}

	pub fn send(&self, session: u64, sub: u64, payload: &[u8]) -> io::Result<()> {
		let mut buf = BytesMut::with_capacity(DGRAM_HEADER + payload.len());
		buf.extend_from_slice(&session.to_be_bytes());
		buf.extend_from_slice(&sub.to_be_bytes());
		buf.extend_from_slice(payload);
		self.conn
			.send_datagram(buf.freeze())
			.map_err(io::Error::other)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use iroh::endpoint::presets;
	use iroh::{EndpointAddr, RelayMode, TransportAddr};

	async fn endpoint<S>(secret: SecretKey, stream: S, remote: EndpointId, accept: bool) -> Endpoint
	where
		S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
	{
		let local = secret.public();
		let transport = Arc::new(SshTransport::new(stream, local, remote));
		let mut builder = Endpoint::builder(presets::N0)
			.secret_key(secret)
			.relay_mode(RelayMode::Disabled)
			.clear_ip_transports()
			.add_custom_transport(transport);
		if accept {
			builder = builder.alpns(vec![REMOWT_ALPN.to_vec()]);
		}
		builder.bind().await.expect("bind")
	}

	#[tokio::test]
	async fn echo_over_ssh_transport() {
		let (client_pipe, agent_pipe) = tokio::io::duplex(256 * 1024);
		let client_secret = SecretKey::generate();
		let agent_secret = SecretKey::generate();
		let client_id = client_secret.public();
		let agent_id = agent_secret.public();

		let client = endpoint(client_secret, client_pipe, agent_id, false).await;
		let agent = endpoint(agent_secret, agent_pipe, client_id, true).await;

		let server = tokio::spawn(async move {
			let incoming = agent.accept().await.expect("incoming");
			let conn = incoming
				.accept()
				.expect("accept")
				.await
				.expect("connecting");
			assert_eq!(conn.remote_id(), client_id);
			let (mut send, mut recv) = conn.accept_bi().await.expect("accept_bi");
			let mut buf = [0u8; 5];
			recv.read_exact(&mut buf).await.expect("read");
			send.write_all(&buf).await.expect("write");
			send.finish().expect("finish");
			conn.closed().await;
		});

		let addr =
			EndpointAddr::from_parts(agent_id, [TransportAddr::Custom(ssh_custom_addr(agent_id))]);
		let conn = client.connect(addr, REMOWT_ALPN).await.expect("connect");
		assert_eq!(conn.remote_id(), agent_id);
		let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
		send.write_all(b"hello").await.expect("write");
		send.finish().expect("finish");
		let mut buf = [0u8; 5];
		recv.read_exact(&mut buf).await.expect("read");
		assert_eq!(&buf, b"hello");

		conn.close(0u32.into(), b"done");
		let _ = server.await;
	}

	#[tokio::test]
	async fn dgram_round_trip() {
		let (client_pipe, agent_pipe) = tokio::io::duplex(256 * 1024);
		let client_secret = SecretKey::generate();
		let agent_secret = SecretKey::generate();
		let client_id = client_secret.public();
		let agent_id = agent_secret.public();

		let client = endpoint(client_secret, client_pipe, agent_id, false).await;
		let agent = endpoint(agent_secret, agent_pipe, client_id, true).await;

		let server = tokio::spawn(async move {
			let incoming = agent.accept().await.expect("incoming");
			let conn = incoming
				.accept()
				.expect("accept")
				.await
				.expect("connecting");
			let router = DatagramRouter::spawn(conn);
			let mut rx = router.register(7);
			if let Some((sub, payload)) = rx.recv().await {
				router.send(7, sub, &payload).expect("send reply");
			}
			tokio::time::sleep(Duration::from_millis(200)).await;
		});

		let addr =
			EndpointAddr::from_parts(agent_id, [TransportAddr::Custom(ssh_custom_addr(agent_id))]);
		let conn = client.connect(addr, REMOWT_ALPN).await.expect("connect");
		let router = DatagramRouter::spawn(conn);
		let mut rx = router.register(7);
		router.send(7, 42, b"ping").expect("send");
		let (sub, payload) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
			.await
			.expect("datagram timed out")
			.expect("router closed");
		assert_eq!(sub, 42);
		assert_eq!(&payload[..], b"ping");
		let _ = server.await;
	}
}
