use anyhow::{Context as _, Result, anyhow};
use camino::Utf8PathBuf;
use tabled::Tabled;
use time::UtcDateTime;

use crate::host::ConfigHost;

#[derive(Debug, Clone, Copy)]
pub enum GenerationStorage {
	Deployer,
	Machine,
	Pusher,
}
impl GenerationStorage {
	fn prefix(&self) -> &'static str {
		match self {
			GenerationStorage::Deployer => "deployer.",
			GenerationStorage::Machine => "",
			GenerationStorage::Pusher => "pusher.",
		}
	}
}

#[derive(Tabled, Debug)]
pub struct Generation {
	#[tabled(rename = "ID", format("{}", self.rollback_id()))]
	pub id: u32,
	#[tabled(rename = "Current")]
	pub current: bool,
	#[tabled(rename = "Created at")]
	pub datetime: UtcDateTime,
	#[tabled(format = "{:?}")]
	pub store_path: Utf8PathBuf,
	#[tabled(skip)]
	pub location: GenerationStorage,
}
impl Generation {
	pub fn rollback_id(&self) -> String {
		format!("{}{}", self.location.prefix(), self.id)
	}
}
impl ConfigHost {
	pub async fn list_generations(&self, profile: &str) -> Result<Vec<Generation>> {
		let nix = self.nix_client().await?;
		let raw = nix
			.list_generations(profile.to_owned())
			.await
			.map_err(|e| anyhow!("{e:?}"))?
			.map_err(|e| anyhow!("{e}"))?;
		raw.into_iter()
			.map(|g| {
				let id: u32 =
					g.id.try_into()
						.with_context(|| format!("generation id {} doesn't fit in u32", g.id))?;
				let datetime = UtcDateTime::from_unix_timestamp(g.creation_time_unix)
					.with_context(|| {
						format!("invalid generation timestamp {}", g.creation_time_unix)
					})?;
				Ok(Generation {
					id,
					current: g.current,
					datetime,
					store_path: g.store_path,
					location: GenerationStorage::Machine,
				})
			})
			.collect()
	}
}
