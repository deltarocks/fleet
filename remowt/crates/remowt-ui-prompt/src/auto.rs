use anyhow::bail;
use tracing::debug;
use zbus::fdo::DBusProxy;
use zbus::names::BusName;

use crate::dbus::{DbusPrompterProxy, BUS_NAME, PROMPTER_PATH};
use crate::rofi::RofiPrompter;
use crate::{Prompter, Result, Source};

pub struct AutoPrompter {
	dbus: Option<DbusPrompterProxy<'static>>,
	fallback: RofiPrompter,
}

impl AutoPrompter {
	pub async fn new() -> Self {
		let dbus = match Self::try_dbus().await {
			Ok(p) => Some(p),
			Err(e) => {
				debug!("dbus prompter unavailable, falling back to rofi: {e}");
				None
			}
		};
		Self {
			dbus,
			fallback: RofiPrompter,
		}
	}

	async fn try_dbus() -> anyhow::Result<DbusPrompterProxy<'static>> {
		let conn = zbus::Connection::session().await?;
		let dbus = DBusProxy::new(&conn).await?;
		let name = BusName::try_from(BUS_NAME)?;
		if !dbus.name_has_owner(name).await? {
			bail!("{BUS_NAME} not registered on session bus");
		}
		let proxy = DbusPrompterProxy::builder(&conn)
			.destination(BUS_NAME)?
			.path(PROMPTER_PATH)?
			.build()
			.await?;
		Ok(proxy)
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
		if let Some(dbus) = &self.dbus {
			return Prompter::prompt_enum(dbus, prompt, description, variants, source).await;
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
		if let Some(dbus) = &self.dbus {
			return Prompter::prompt_text(dbus, echo, prompt, description, source).await;
		}
		self.fallback
			.prompt_text(echo, prompt, description, source)
			.await
	}

	async fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()> {
		if let Some(dbus) = &self.dbus {
			return Prompter::display_text(dbus, error, description, source).await;
		}
		self.fallback.display_text(error, description, source).await
	}
}
