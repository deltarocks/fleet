use std::pin::pin;

use anyhow::bail;
use futures::stream::Peekable;
use futures::StreamExt as _;
use remowt_ui_prompt::Prompter;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::select;
use tokio_util::codec::{FramedRead, LinesCodec};

pub async fn run_conversation<R, W, P>(reader: R, mut writer: W, prompt: P) -> anyhow::Result<()>
where
	R: AsyncRead,
	W: AsyncWrite + Unpin,
	P: Prompter,
{
	let mut lines = pin!(FramedRead::new(reader, LinesCodec::new()).peekable());

	while let Some(line) = lines.next().await {
		let line = line?;
		let res = if let Some(prompt_text) = line.strip_prefix("PAM_PROMPT_ECHO_OFF ") {
			prompt.prompt_text(false, prompt_text, "", &[]).await?
		} else if let Some(prompt_text) = line.strip_prefix("PAM_PROMPT_ECHO_ON ") {
			prompt.prompt_text(true, prompt_text, "", &[]).await?
		} else if let Some(msg_text) = line.strip_prefix("PAM_ERROR_MSG ") {
			prompt.display_text(true, msg_text, &[]).await?;
			String::new()
		} else if let Some(msg_text) = line.strip_prefix("PAM_TEXT_INFO ") {
			select! {
				_ = Peekable::peek(lines.as_mut()) => {},
				r = prompt.display_text(false, msg_text, &[]) => {r?}
			}
			String::new()
		} else if line == "SUCCESS" {
			return Ok(());
		} else if line == "FAILURE" {
			bail!("helper reported failure")
		} else {
			bail!("unknown agent request: {line}")
		};

		if res.contains('\n') {
			bail!("response should not include newline")
		}

		writer.write_all(res.as_bytes()).await?;
		writer.write_all(b"\n").await?;
	}
	bail!("agent finished unexpectedly")
}
