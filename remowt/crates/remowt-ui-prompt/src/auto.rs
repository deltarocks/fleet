use std::path::Path;

use bifrostlink::declarative::RemoteEndpoints as _;
use remowt_link_shared::{Address, BifConfig, gateway};
use tracing::debug;

use crate::bifrost::PromptEndpointsClient;
use crate::rofi::RofiPrompter;
use crate::{Prompter, Result, Source};

pub struct AutoPrompter {
	remote: Option<PromptEndpointsClient<BifConfig>>,
	fallback: RofiPrompter,
}

impl AutoPrompter {
	pub async fn new() -> Self {
		let remote = match gateway::local_socket() {
			Ok(path) => Self::try_connect(&path).await,
			Err(e) => {
				debug!("no local gateway socket, falling back to rofi: {e}");
				None
			}
		};
		Self {
			remote,
			fallback: RofiPrompter,
		}
	}

	async fn try_connect(path: &Path) -> Option<PromptEndpointsClient<BifConfig>> {
		match gateway::connect(path).await {
			Ok(rpc) => Some(PromptEndpointsClient::wrap(rpc.remote(Address::User))),
			Err(e) => {
				debug!("local prompt agent unavailable, falling back to rofi: {e}");
				None
			}
		}
	}
}

impl Prompter for AutoPrompter {
	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> Result<u32> {
		if let Some(remote) = &self.remote {
			return Prompter::prompt_enum(remote, prompt, description, variants, source).await;
		}
		self.fallback
			.prompt_enum(prompt, description, variants, source)
			.await
	}

	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<String> {
		if let Some(remote) = &self.remote {
			return Prompter::prompt_text(remote, echo, prompt, description, source).await;
		}
		self.fallback
			.prompt_text(echo, prompt, description, source)
			.await
	}

	async fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()> {
		if let Some(remote) = &self.remote {
			return Prompter::display_text(remote, error, description, source).await;
		}
		self.fallback.display_text(error, description, source).await
	}
}
