use std::process::Stdio;

use anyhow::{anyhow, bail};
use nix::unistd::User;
use remowt_polkit_shared::Identity;
use remowt_ui_prompt::Prompter;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use super::protocol::run_conversation;
use super::Helper;

#[derive(Clone)]
pub struct SuidHelper;
impl Helper for SuidHelper {
	async fn help_me<P: Prompter + 'static>(
		&self,
		cookie: &str,
		prompt: P,
		identity: Identity,
	) -> anyhow::Result<()> {
		let Some(uid) = identity.uid() else {
			bail!("can't process identity");
		};
		let user = User::from_uid(uid)
			.map_err(|e| anyhow!("error querying user: {e}"))?
			.ok_or_else(|| anyhow!("user not found"))?;

		let mut cmd = Command::new("polkit-agent-helper-1");
		cmd.arg(user.name);
		cmd.stdin(Stdio::piped());
		cmd.stdout(Stdio::piped());
		cmd.kill_on_drop(true);
		let mut child = cmd.spawn()?;
		let mut stdin = child.stdin.take().expect("piped");
		let stdout = child.stdout.take().expect("piped");

		assert!(!cookie.contains('\n'));
		stdin.write_all(cookie.as_bytes()).await?;
		stdin.write_all(b"\n").await?;

		let res = run_conversation(stdout, stdin, prompt).await;
		drop(child);
		res
	}
}
