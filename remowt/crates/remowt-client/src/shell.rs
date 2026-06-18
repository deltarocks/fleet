use anyhow::{anyhow, Result};
use bifrostlink::declarative::RemoteEndpoints as _;
use bifrostlink::Remote;
use remowt_endpoints::pty::{PtyClient, ShellId};
use remowt_link_shared::{Address, BifConfig};

use crate::forwarded::RemowtStream;
use crate::Remowt;

pub struct RemowtShell {
	pub id: ShellId,
	pub stream: RemowtStream,
	remote: Remote<BifConfig>,
}
impl RemowtShell {
	pub fn resizer(&self) -> RemowtShellResizer {
		RemowtShellResizer {
			remote: self.remote.clone(),
			id: self.id,
		}
	}
}

#[derive(Clone)]
pub struct RemowtShellResizer {
	remote: Remote<BifConfig>,
	id: ShellId,
}

impl RemowtShellResizer {
	pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
		PtyClient::wrap(self.remote.clone())
			.resize(self.id, cols, rows)
			.await?
			.map_err(|e| anyhow!("failed to resize remote shell: {e}"))
	}
}

impl Remowt {
	pub async fn open_shell(
		&self,
		term: &str,
		cols: u16,
		rows: u16,
		escalate: bool,
	) -> Result<RemowtShell> {
		let (forwarded, tunnel) = self.bind_fast_tunnel("shell", escalate).await?;
		let client: PtyClient<BifConfig> = if escalate {
			self.run0_endpoints().await?
		} else {
			self.endpoints()
		};
		let id = client
			.open_shell(tunnel, term.to_owned(), cols, rows)
			.await?
			.map_err(|e| anyhow!("agent failed to open shell: {e}"))?;
		let stream = forwarded.accept().await?;

		Ok(RemowtShell {
			id,
			stream,
			remote: self.0.rpc.remote(Address::Agent),
		})
	}
}
