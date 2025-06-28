use std::str::FromStr as _;

use age::Recipient;
use anyhow::{Result, anyhow};
use futures::{StreamExt as _, TryStreamExt as _};
use itertools::Itertools as _;
use tracing::warn;

use crate::host::Config;

impl Config {
	pub fn cached_key(&self, host: &str) -> Option<String> {
		let data = self.data();
		let key = data.hosts.get(host).map(|h| &h.encryption_key);
		if let Some(key) = key {
			if key.is_empty() {
				return None;
			}
		}
		key.cloned()
	}
	pub fn update_key(&self, host: &str, key: String) {
		let mut data = self.data_mut();
		let host = data.hosts.entry(host.to_string()).or_default();
		host.encryption_key = key.trim().to_string();
	}

	pub async fn key(&self, host: &str) -> anyhow::Result<String> {
		if let Some(key) = self.cached_key(host) {
			Ok(key)
		} else {
			warn!("Loading key for {}", host);
			let host = self.host(host).await?;
			let mut cmd = host.cmd("cat").await?;
			cmd.arg("/etc/ssh/ssh_host_ed25519_key.pub");
			let key = cmd.run_string().await?;
			self.update_key(&host.name, key.clone());
			Ok(key)
		}
	}
	/// Insecure, requires root
	pub async fn recipient(&self, host: &str) -> anyhow::Result<impl Recipient + use<>> {
		let key = self.key(host).await?;
		age::ssh::Recipient::from_str(&key).map_err(|e| anyhow!("parse recipient error: {:?}", e))
	}

	pub async fn recipients(&self, hosts: Vec<String>) -> Result<Vec<impl Recipient + use<>>> {
		let hosts = self.expand_owner_set(hosts).await?;
		futures::stream::iter(hosts.iter())
			.then(|m| self.recipient(m.as_ref()))
			.try_collect::<Vec<_>>()
			.await
	}

	#[allow(dead_code)]
	pub async fn orphaned_data(&self) -> Result<Vec<String>> {
		let mut out = Vec::new();
		let host_names = self
			.list_hosts()
			.await?
			.into_iter()
			.map(|h| h.name)
			.collect_vec();
		for hostname in self
			.data()
			.hosts
			.iter()
			.filter(|(_, host)| !host.encryption_key.is_empty())
			.map(|(n, _)| n)
		{
			if !host_names.contains(hostname) {
				out.push(hostname.to_owned())
			}
		}

		Ok(out)
	}
}
