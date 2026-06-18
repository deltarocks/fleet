use std::io;

use bifrostlink::Port;
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::select;

pub fn child_port<R, W>(mut reader: R, mut writer: W) -> Port
where
	R: AsyncRead + Unpin + Send + 'static,
	W: AsyncWrite + Unpin + Send + 'static,
{
	Port::new(|mut rx, tx| async move {
		let read_task = async move {
			loop {
				let len = match reader.read_u32().await {
					Ok(len) => len,
					Err(e) => {
						log_read_end(&e);
						break;
					}
				};
				let mut buf = BytesMut::zeroed(len as usize);
				if let Err(e) = reader.read_exact(&mut buf).await {
					log_read_end(&e);
					break;
				}
				if tx.send(buf.freeze()).is_err() {
					break;
				}
			}
		};
		let write_task = async move {
			while let Some(msg) = rx.recv().await {
				if let Err(e) = write_frame(&mut writer, msg).await {
					log_write_end(&e);
					break;
				}
			}
		};
		select! {
			_ = read_task => {},
			_ = write_task => {},
		}
	})
}

fn log_read_end(e: &io::Error) {
	if matches!(
		e.kind(),
		io::ErrorKind::UnexpectedEof
			| io::ErrorKind::BrokenPipe
			| io::ErrorKind::ConnectionReset
			| io::ErrorKind::ConnectionAborted
	) {
		tracing::debug!("child read ended: {e}");
	} else {
		tracing::error!("child read failed: {e}");
	}
}

fn log_write_end(e: &io::Error) {
	if matches!(
		e.kind(),
		io::ErrorKind::BrokenPipe
			| io::ErrorKind::ConnectionReset
			| io::ErrorKind::ConnectionAborted
	) {
		tracing::debug!("child write ended: {e}");
	} else {
		tracing::error!("child write failed: {e}");
	}
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, msg: Bytes) -> io::Result<()> {
	let len = u32::try_from(msg.len())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message larger than 4GB"))?;
	writer.write_u32(len).await?;
	writer.write_all(&msg).await?;
	writer.flush().await?;
	Ok(())
}
