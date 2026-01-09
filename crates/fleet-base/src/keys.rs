use std::str::FromStr as _;

use age::Recipient;
use anyhow::{Result, anyhow, bail};
use futures::{StreamExt as _, TryStreamExt as _};
use itertools::Itertools as _;
use tracing::warn;

use crate::{fleetdata::SecretOwner, host::Config};

impl Config {
	fn cached_host_key(&self, host: &str) -> Option<String> {
		let hosts = self.data.hosts.read().expect("no poisoning");
		let key = hosts.get(host).map(|h| &h.encryption_key);
		if let Some(key) = key
			&& key.is_empty()
		{
			return None;
		}
		key.cloned()
	}
	pub fn update_key(&self, host: &str, key: String) {
		let mut hosts = self.data.hosts.write().expect("no poisoning");
		let host = hosts.entry(host.to_string()).or_default();
		host.encryption_key = key.trim().to_string();
	}

	pub async fn host_key(&self, host: &str) -> anyhow::Result<String> {
		if let Some(key) = self.cached_host_key(host) {
			Ok(key)
		} else {
			warn!("Loading key for {}", host);
			let host = self.host(host)?;
			let mut cmd = host.cmd("cat").await?;
			cmd.arg("/etc/ssh/ssh_host_ed25519_key.pub");
			let key = cmd.run_string().await?;
			self.update_key(&host.name, key.clone());
			Ok(key)
		}
	}
	pub async fn key(&self, owner: &SecretOwner) -> anyhow::Result<String> {
		if let Some(host) = owner.as_host() {
			self.host_key(host).await
		} else {
			bail!("only host keys supported for now")
		}
	}
	/// Insecure, requires root
	pub async fn recipient(&self, host: &SecretOwner) -> anyhow::Result<Box<dyn Recipient>> {
		let key = self.key(host).await?;
		age::ssh::Recipient::from_str(&key)
			.map_err(|e| anyhow!("parse recipient error: {e:?}"))
			.map(|v| Box::new(v) as Box<dyn Recipient>)
	}

	pub async fn recipients(&self, hosts: Vec<SecretOwner>) -> Result<Vec<Box<dyn Recipient>>> {
		futures::stream::iter(hosts.iter())
			.then(|m| self.recipient(m))
			.try_collect::<Vec<_>>()
			.await
	}

	#[allow(dead_code)]
	pub async fn orphaned_data(&self) -> Result<Vec<String>> {
		let mut out = Vec::new();
		let host_names = self.list_hosts()?.into_iter().map(|h| h.name).collect_vec();
		let hosts = self.data.hosts.read().expect("no poisoning");
		for hostname in hosts
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
