use zbus::interface;
use zbus::{fdo, proxy};

use crate::Source;
use crate::{BlockingPrompter, Result};
use crate::{Error, Prompter};

pub const BUS_NAME: &str = "lach.RemowtAskpass";
pub const PROMPTER_PATH: &str = "/lach/Askpass";

pub struct DbusPrompterInterface<P>(pub P);

#[interface(name = "lach.PolkitInputHandler")]
impl<P: Prompter + Send + Sync + 'static> DbusPrompterInterface<P> {
	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: Vec<String>,
		source: Vec<Source>,
	) -> fdo::Result<u32> {
		let variants: Vec<&str> = variants.iter().map(|v| v.as_str()).collect();
		Ok(self
			.0
			.prompt_enum(prompt, description, &variants, &source)
			.await?)
	}
	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: Vec<Source>,
	) -> fdo::Result<String> {
		Ok(self
			.0
			.prompt_text(echo, prompt, description, &source)
			.await?)
	}
	async fn display_text(
		&self,
		error: bool,
		description: &str,
		source: Vec<Source>,
	) -> fdo::Result<()> {
		Ok(self.0.display_text(error, description, &source).await?)
	}
}

#[proxy(interface = "lach.PolkitInputHandler")]
pub trait DbusPrompter {
	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> fdo::Result<u32>;
	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> fdo::Result<String>;
	async fn display_text(
		&self,
		error: bool,
		description: &str,
		source: &[Source],
	) -> fdo::Result<()>;
}

impl Prompter for DbusPrompterProxy<'_> {
	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> Result<u32> {
		Ok(self
			.prompt_enum(prompt, description, variants, source)
			.await?)
	}

	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<String> {
		Ok(self.prompt_text(echo, prompt, description, source).await?)
	}

	async fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()> {
		Ok(self.display_text(error, description, source).await?)
	}
}
impl BlockingPrompter for DbusPrompterProxyBlocking<'_> {
	fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> Result<u32> {
		Ok(self.prompt_enum(prompt, description, variants, source)?)
	}

	fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<String> {
		Ok(self.prompt_text(echo, prompt, description, source)?)
	}

	fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()> {
		Ok(self.display_text(error, description, source)?)
	}
}

impl From<fdo::Error> for Error {
	fn from(value: fdo::Error) -> Self {
		if matches!(value, fdo::Error::NoReply(_)) {
			return Self::Cancel;
		}
		Self::InputError(format!("{value}"))
	}
}
impl From<Error> for fdo::Error {
	fn from(value: Error) -> Self {
		match value {
			Error::Cancel => fdo::Error::NoReply("input was cancelled".to_owned()),
			Error::Remote(e) => fdo::Error::NoReply(format!("remote error occured: {e}")),
			Error::InputError(e) => fdo::Error::Failed(e),
		}
	}
}
