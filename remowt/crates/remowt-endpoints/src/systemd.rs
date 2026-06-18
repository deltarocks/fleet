use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use serde::{Deserialize, Serialize};
use zbus::proxy;
use zbus::zvariant::OwnedObjectPath;

pub struct Systemd;

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("systemd request failed: {0}")]
	Failed(String),
}

#[proxy(
	interface = "org.freedesktop.systemd1.Manager",
	default_service = "org.freedesktop.systemd1",
	default_path = "/org/freedesktop/systemd1"
)]
trait Manager {
	fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
	fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
}

async fn manager() -> Result<ManagerProxy<'static>, Error> {
	let conn = zbus::Connection::system()
		.await
		.map_err(|e| Error::Failed(e.to_string()))?;
	ManagerProxy::new(&conn)
		.await
		.map_err(|e| Error::Failed(e.to_string()))
}

#[endpoints(ns = 5)]
impl Systemd {
	#[endpoints(id = 1)]
	async fn start(&self, unit: String) -> Result<(), Error> {
		manager()
			.await?
			.start_unit(&unit, "replace")
			.await
			.map_err(|e| Error::Failed(e.to_string()))?;
		Ok(())
	}
	#[endpoints(id = 2)]
	async fn stop(&self, unit: String) -> Result<(), Error> {
		manager()
			.await?
			.stop_unit(&unit, "replace")
			.await
			.map_err(|e| Error::Failed(e.to_string()))?;
		Ok(())
	}
}
