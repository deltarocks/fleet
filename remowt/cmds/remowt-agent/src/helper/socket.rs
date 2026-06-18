use anyhow::{anyhow, bail};
use nix::unistd::User;
use remowt_polkit_shared::Identity;
use remowt_ui_prompt::Prompter;
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;
use tracing::debug;

use super::protocol::run_conversation;
use super::Helper;

/// Polkit 127 introduced an alternative backend similar to `lach.PolkitHelper`
const SOCKET_PATH: &str = "/run/polkit/agent-helper.socket";

#[derive(Clone)]
pub struct SocketHelper<F> {
	pub fallback: F,
}

impl<F: Helper + Sync> Helper for SocketHelper<F> {
	async fn help_me<P: Prompter + Send + Sync + 'static>(
		&self,
		cookie: &str,
		prompt: P,
		identity: Identity,
	) -> anyhow::Result<()> {
		let Some(uid) = identity.uid() else {
			bail!("can't process identity");
		};

		let stream = match UnixStream::connect(SOCKET_PATH).await {
			Ok(stream) => stream,
			Err(e) => {
				debug!("agent-helper.socket unavailable ({e}), using fallback helper");
				return self.fallback.help_me(cookie, prompt, identity).await;
			}
		};

		let user = User::from_uid(uid)
			.map_err(|e| anyhow!("error querying user: {e}"))?
			.ok_or_else(|| anyhow!("user not found"))?;

		assert!(!cookie.contains('\n'));
		let (reader, mut writer) = stream.into_split();

		writer.write_all(user.name.as_bytes()).await?;
		writer.write_all(b"\n").await?;
		writer.write_all(cookie.as_bytes()).await?;
		writer.write_all(b"\n").await?;

		run_conversation(reader, writer, prompt).await
	}
}
