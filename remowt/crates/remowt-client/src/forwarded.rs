use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{anyhow, Result};
use camino::Utf8PathBuf;
use remowt_link_shared::iroh_tunnel::IrohBiStream;
use russh::client::Msg;
use russh::{Channel, ChannelStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

pub enum RemowtListener {
	Ssh(oneshot::Receiver<Channel<Msg>>),
	Local(UnixListener, Utf8PathBuf),
	Iroh(oneshot::Receiver<RemowtStream>),
}

impl RemowtListener {
	pub async fn accept(self) -> Result<RemowtStream> {
		match self {
			RemowtListener::Ssh(rx) => {
				let ch = rx
					.await
					.map_err(|_| anyhow!("agent never connected the forwarded socket"))?;
				Ok(RemowtStream::Ssh(ch.into_stream()))
			}
			RemowtListener::Local(listener, path) => {
				let (stream, _) = listener.accept().await?;
				let _ = std::fs::remove_file(&path);
				Ok(RemowtStream::Local(stream))
			}
			RemowtListener::Iroh(rx) => rx
				.await
				.map_err(|_| anyhow!("agent never opened the iroh tunnel")),
		}
	}
}

pub enum RemowtStream {
	Ssh(ChannelStream<Msg>),
	Local(UnixStream),
	Iroh(IrohBiStream),
}

impl AsyncRead for RemowtStream {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		match self.get_mut() {
			RemowtStream::Ssh(s) => Pin::new(s).poll_read(cx, buf),
			RemowtStream::Local(s) => Pin::new(s).poll_read(cx, buf),
			RemowtStream::Iroh(s) => Pin::new(s).poll_read(cx, buf),
		}
	}
}

impl AsyncWrite for RemowtStream {
	fn poll_write(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		match self.get_mut() {
			RemowtStream::Ssh(s) => Pin::new(s).poll_write(cx, buf),
			RemowtStream::Local(s) => Pin::new(s).poll_write(cx, buf),
			RemowtStream::Iroh(s) => Pin::new(s).poll_write(cx, buf),
		}
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		match self.get_mut() {
			RemowtStream::Ssh(s) => Pin::new(s).poll_flush(cx),
			RemowtStream::Local(s) => Pin::new(s).poll_flush(cx),
			RemowtStream::Iroh(s) => Pin::new(s).poll_flush(cx),
		}
	}

	fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		match self.get_mut() {
			RemowtStream::Ssh(s) => Pin::new(s).poll_shutdown(cx),
			RemowtStream::Local(s) => Pin::new(s).poll_shutdown(cx),
			RemowtStream::Iroh(s) => Pin::new(s).poll_shutdown(cx),
		}
	}
}
