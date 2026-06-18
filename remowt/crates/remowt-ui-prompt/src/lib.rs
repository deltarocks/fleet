use core::fmt;
use std::borrow::Cow;
use std::future::Future;
use std::result;

pub mod auto;
pub mod bifrost;
pub mod rofi;

#[derive(thiserror::Error, Debug, serde::Serialize, serde::Deserialize)]
pub enum Error {
	#[error("user has cancelled input")]
	Cancel,
	#[error("input error: {0}")]
	InputError(String),
	#[error("unknown remote error: {0}")]
	Remote(String),
}

pub type Result<T, E = Error> = result::Result<T, E>;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Source(pub Cow<'static, str>);
impl fmt::Display for Source {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "<u>{}</u>", self.0)
	}
}

pub trait Prompter: Send + Sync {
	fn prompt_radio(
		&self,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> impl Future<Output = Result<bool>> + Send {
		let fut = self.prompt_enum(prompt, description, &["No", "Yes"], source);
		async { fut.await.map(|v| v == 1) }
	}
	fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> impl Future<Output = Result<u32>> + Send;
	fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> impl Future<Output = Result<String>> + Send;
	fn display_text(
		&self,
		error: bool,
		description: &str,
		source: &[Source],
	) -> impl Future<Output = Result<()>> + Send;
}
pub trait BlockingPrompter {
	fn prompt_radio(&self, prompt: &str, description: &str, source: &[Source]) -> Result<bool> {
		self.prompt_enum(prompt, description, &["No", "Yes"], source)
			.map(|v| v == 1)
	}
	fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> Result<u32>;
	fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<String>;
	fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()>;
}
impl<P> Prompter for &P
where
	P: Prompter,
{
	fn prompt_radio(
		&self,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> impl Future<Output = Result<bool>> + Send {
		(*self).prompt_radio(prompt, description, source)
	}

	fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> impl Future<Output = Result<u32>> + Send {
		(*self).prompt_enum(prompt, description, variants, source)
	}

	fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> impl Future<Output = Result<String>> + Send {
		(*self).prompt_text(echo, prompt, description, source)
	}

	fn display_text(
		&self,
		error: bool,
		description: &str,
		source: &[Source],
	) -> impl Future<Output = Result<()>> + Send {
		(*self).display_text(error, description, source)
	}
}

pub struct PrependSourcePrompter<P> {
	pub prompter: P,
	pub source: Vec<Source>,
	pub description: String,
}
impl<P> PrependSourcePrompter<P> {
	fn source(&self, input: &[Source]) -> Vec<Source> {
		let mut out = self.source.clone();
		out.extend(input.iter().cloned());
		out
	}
	fn description(&self, input: &str) -> String {
		if self.description.is_empty() {
			input.to_owned()
		} else if input.is_empty() {
			self.description.to_owned()
		} else {
			format!("{input}\n\n{}", self.description)
		}
	}
}
impl<P> Prompter for PrependSourcePrompter<P>
where
	P: Prompter + Sync,
{
	async fn prompt_radio(
		&self,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<bool> {
		self.prompter
			.prompt_radio(prompt, &self.description(description), &self.source(source))
			.await
	}

	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> Result<u32> {
		self.prompter
			.prompt_enum(
				prompt,
				&self.description(description),
				variants,
				&self.source(source),
			)
			.await
	}

	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<String> {
		self.prompter
			.prompt_text(
				echo,
				prompt,
				&self.description(description),
				&self.source(source),
			)
			.await
	}

	async fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()> {
		self.prompter
			.display_text(error, &self.description(description), &self.source(source))
			.await
	}
}
