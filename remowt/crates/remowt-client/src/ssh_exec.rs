use bytes::Bytes;
use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::sync::oneshot;

const BUF: usize = 64 * 1024;

pub(crate) struct SshExecChild {
	pub stdin: DuplexStream,
	pub stdout: DuplexStream,
	pub stderr: DuplexStream,
	pub exit: oneshot::Receiver<Option<u32>>,
}

impl SshExecChild {
	/// Manage channel returned by russh exec().
	pub(crate) fn from_exec(ch: Channel<Msg>) -> Self {
		let (stdin, mut stdin_r) = tokio::io::duplex(BUF);
		let (mut out_w, stdout) = tokio::io::duplex(BUF);
		let (mut err_w, stderr) = tokio::io::duplex(BUF);
		let (exit_tx, exit) = oneshot::channel();

		tokio::spawn(async move {
			let (mut read, write) = ch.split();

			let stdin_pump = tokio::spawn(async move {
				let mut buf = vec![0u8; BUF];
				loop {
					match stdin_r.read(&mut buf).await {
						Ok(0) | Err(_) => break,
						Ok(n) => {
							if write
								.data_bytes(Bytes::copy_from_slice(&buf[..n]))
								.await
								.is_err()
							{
								return;
							}
						}
					}
				}
				let _ = write.eof().await;
			});

			let mut code = None;
			while let Some(msg) = read.wait().await {
				match msg {
					ChannelMsg::Data { data } => {
						if out_w.write_all(&data).await.is_err() {
							break;
						}
					}
					ChannelMsg::ExtendedData { data, .. } => {
						if err_w.write_all(&data).await.is_err() {
							break;
						}
					}
					ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
					_ => {}
				}
			}

			stdin_pump.abort();
			let _ = out_w.shutdown().await;
			let _ = err_w.shutdown().await;
			let _ = exit_tx.send(code);
		});

		SshExecChild {
			stdin,
			stdout,
			stderr,
			exit,
		}
	}

	/// Wait for the process to finish, returning its exit status.
	pub(crate) async fn wait(self) -> Option<u32> {
		self.exit.await.ok().flatten()
	}
}
