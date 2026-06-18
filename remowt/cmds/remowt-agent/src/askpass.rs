use std::borrow::Cow;
use std::io::Write as _;

use anyhow::Context as _;
use remowt_ui_prompt::dbus::{DbusPrompterInterface, DbusPrompterProxy, BUS_NAME, PROMPTER_PATH};
use remowt_ui_prompt::{Prompter, Source};
use tracing::debug;
use zbus::Connection;

pub async fn serve<P>(conn: &Connection, prompter: P) -> anyhow::Result<()>
where
	P: Prompter + 'static,
{
	conn.object_server()
		.at(PROMPTER_PATH, DbusPrompterInterface(prompter))
		.await?;
	match conn.request_name(BUS_NAME).await {
		Ok(()) => {}
		Err(zbus::Error::NameTaken) => {
			debug!("{BUS_NAME} already owned, chaining to upstream");
		}
		Err(e) => return Err(e.into()),
	}
	Ok(())
}

pub async fn ask(prompt: &str, description: String) -> anyhow::Result<()> {
	let conn = Connection::session()
		.await
		.context("connecting to the session bus (DBUS_SESSION_BUS_ADDRESS)")?;
	let proxy = DbusPrompterProxy::builder(&conn)
		.destination(BUS_NAME)?
		.path(PROMPTER_PATH)?
		.build()
		.await?;

	let password = proxy
		.prompt_text(
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
