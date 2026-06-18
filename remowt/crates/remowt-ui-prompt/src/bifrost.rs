use bifrostlink::{Config, Rpc};
use bifrostlink_macros::endpoints;
use serde::{Deserialize, Serialize};

use crate::{Error, Prompter, Source};

pub struct PromptEndpoints<P>(pub P);

#[endpoints(ns = 2)]
impl<P> PromptEndpoints<P>
where
	P: Prompter + Send + Sync + 'static,
{
	#[endpoints(id = 1, cancel)]
	async fn prompt_enum(
		&self,
		prompt: String,
		description: String,
		variants: Vec<String>,
		source: Vec<Source>,
	) -> Result<u32, Error> {
		let variants: Vec<&str> = variants.iter().map(|v| v.as_str()).collect();
		self.0
			.prompt_enum(&prompt, &description, &variants, &source)
			.await
	}

	#[endpoints(id = 2, cancel)]
	async fn prompt_text(
		&self,
		echo: bool,
		prompt: String,
		description: String,
		source: Vec<Source>,
	) -> Result<String, Error> {
		self.0
			.prompt_text(echo, &prompt, &description, &source)
			.await
	}

	#[endpoints(id = 3, cancel)]
	async fn display_text(
		&self,
		error: bool,
		description: String,
		source: Vec<Source>,
	) -> Result<(), Error> {
		self.0.display_text(error, &description, &source).await
	}
}

impl<C: Config> Prompter for PromptEndpointsClient<C>
where
	Error: ToString,
{
	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> crate::Result<u32> {
		self.prompt_enum(
			prompt.to_owned(),
			description.to_owned(),
			variants.iter().map(|v| (*v).to_owned()).collect(),
			source.to_vec(),
		)
		.await
		.map_err(|e| Error::Remote(e.to_string()))?
	}

	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> crate::Result<String> {
		self.prompt_text(
			echo,
			prompt.to_owned(),
			description.to_owned(),
			source.to_vec(),
		)
		.await
		.map_err(|e| Error::Remote(e.to_string()))?
	}

	async fn display_text(
		&self,
		error: bool,
		description: &str,
		source: &[Source],
	) -> crate::Result<()> {
		self.display_text(error, description.to_owned(), source.to_vec())
			.await
			.map_err(|e| Error::Remote(e.to_string()))?
	}
}

pub fn serve_prompts<P, C>(rpc: &mut Rpc<C>, prompt: P)
where
	P: Prompter + Send + Sync + 'static,
	C: Config,
	C::Error: From<Error>,
{
	PromptEndpoints(prompt).register_endpoints(rpc);
}
