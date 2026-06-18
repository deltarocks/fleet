use std::borrow::Cow;
use std::io::Write as _;

use bifrostlink::declarative::RemoteEndpoints as _;
use remowt_link_shared::{gateway, Address, BifConfig};
use remowt_ui_prompt::bifrost::PromptEndpointsClient;
use remowt_ui_prompt::{Prompter, Source};

pub async fn ask(prompt: &str, description: String) -> anyhow::Result<()> {
	let rpc = gateway::connect(&gateway::socket_path()?).await?;
	let prompter = PromptEndpointsClient::<BifConfig>::wrap(rpc.remote(Address::User));
	let password = Prompter::prompt_text(
		&prompter,
		false,
		prompt,
		&description,
		&[Source(Cow::Borrowed("remowt-askpass"))],
	)
	.await?;

	let mut out = std::io::stdout().lock();
	out.write_all(password.as_bytes())?;
	out.write_all(b"\n")?;
	out.flush()?;
	Ok(())
}
