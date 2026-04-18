use std::collections::BTreeSet;
use std::io::{Write as _, stdout};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser;
use fleet_base::{fleetdata::SecretOwner, host::Config, opts::FleetOpts};
use itertools::Itertools as _;
use nix_eval::nix_go;
use tracing::{info, warn};

#[derive(Parser)]
pub enum Secret {
	/// Force load host keys for all defined hosts
	ForceKeys,
	/// Read secret from remote host, requires sudo on one of the owning hosts
	Read {
		/// Secret name to read
		name: String,

		/// Distribution with what machine to read
		/// If not shared between multiple - defaults to single owner
		#[clap(short = 'm', long)]
		machine: Option<String>,

		/// Which private secret part to read
		/// If not specified - only one existing part is read
		#[clap(short = 'p', long)]
		part: Option<String>,
	},
	/// Prune (remove, mark for regeneration) secrets
	Prune {
		/// Secret to prune
		name: String,

		/// Machines to prune - if specified, only the choosen machines will be pruned
		#[clap(short = 'm', long)]
		machine: Vec<String>,

		/// If set - distributions containing the specified machines will be pruned fully
		#[clap(long)]
		whole_dist: bool,
	},
	/// Ensure secret is generated and not expired
	Ensure {
		/// Secret to ensure generated
		name: String,

		/// Machines to force secret for
		#[clap(short = 'm', long)]
		machine: Vec<String>,
	},
	/// Regenerate secret (prune then ensure)
	Regenerate {
		/// Secret to regenerate
		name: String,

		/// Machines to regenerate for - if specified, only those machines
		#[clap(short = 'm', long)]
		machine: Vec<String>,
	},
	List {},
}

impl Secret {
	pub async fn run(self, config: &Config, opts: &FleetOpts) -> Result<()> {
		match self {
			Secret::ForceKeys => {
				for host in config.list_hosts()? {
					if opts.should_skip(&host)? {
						continue;
					}
					config.host_key(&host.name).await?;
				}
			}
			Secret::Read {
				name,
				machine,
				part: part_name,
			} => {
				let (owners, secret_data) = {
					let secret = config.data.secrets.read().expect("not poisoned");

					let Some(dist) = secret.get(&name) else {
						bail!("secret doesn't exists");
					};

					let dist = if let Some(machine) = &machine {
						dist.get(&SecretOwner::host(machine))
							.ok_or_else(|| anyhow!("machine {machine} has no secret generated"))?
					} else {
						dist.distributions()
							.exactly_one()
							.map_err(|e| anyhow!("{e}"))
							.context(
								"with no machine specified, there should be exactly one distribution",
							)?
					};

					let part = if let Some(part_name) = &part_name {
						dist.secret.parts.get(part_name).ok_or_else(|| {
							anyhow!("secret {name} does not have part named {part_name}")
						})?
					} else {
						dist.secret
							.parts
							.iter()
							.exactly_one()
							.map_err(|e| anyhow!("{e}"))
							.context("with no part specified, there should be exactly one part")?
							.1
					};
					let owners = dist.owners().cloned().collect::<Vec<_>>();
					let secret_data = part.raw.clone();
					(owners, secret_data)
				};

				for host in config
					.preferred_hosts(|h| owners.iter().any(|o| o.as_host() == Some(h)))
					.context("failed to list hosts")?
				{
					let host = match host {
						Ok(h) => h,
						Err(e) => {
							warn!("failed to use host: {e}");
							continue;
						}
					};
					match host.decrypt(secret_data.clone()).await {
						Ok(data) => {
							let mut w = stdout();
							w.write_all(&data)?;
							return Ok(());
						}
						Err(e) => warn!("failed to decrypt on {}: {e}", host.name),
					};
				}
				bail!("failed to find suitable decrypting host");
			}
			Secret::List {} => {
				let secrets = config.data.secrets.read().expect("not poisoned");

				#[derive(tabled::Tabled)]
				struct Row {
					#[tabled(rename = "Name")]
					name: String,
					#[tabled(rename = "Dist")]
					dist: String,
					#[tabled(rename = "Owners")]
					owners: String,
				}

				let mut rows = Vec::new();
				for name in secrets.keys() {
					let dists = secrets.get(name).unwrap();
					for (idx, dist) in dists.all_distributions().enumerate() {
						let active: Vec<_> = dist
							.owners()
							.filter_map(|o| o.as_host())
							.map(str::to_owned)
							.collect();
						let pruned: Vec<_> = dist
							.owners_pending_prune()
							.filter_map(|o| o.as_host())
							.map(|h| format!("{h} (pruned)"))
							.collect();
						let mut all_owners = active;
						all_owners.extend(pruned);

						let dist_label = if dist.is_pending_prune() {
							format!("{idx} (pruned)")
						} else {
							idx.to_string()
						};

						rows.push(Row {
							name: if idx == 0 {
								name.clone()
							} else {
								String::new()
							},
							dist: dist_label,
							owners: all_owners.join("\n"),
						});
					}
				}

				use tabled::settings::{Style, width::Width};
				let mut table = tabled::Table::new(rows);
				table.with(Width::wrap(80));
				println!("{table}");
			}
			Secret::Prune {
				name,
				machine,
				whole_dist,
			} => {
				Self::prune(config, &name, &machine, whole_dist)?;
			}
			Secret::Ensure { name, machine } => {
				Self::ensure(config, opts, &name, &machine)?;
			}
			Secret::Regenerate { name, machine } => {
				let pruned = Self::prune(config, &name, &machine, true)?;
				// In general, this is not correct - already evaluated secret would still be cached after pruning
				// But as a dedicated CLI subcommand it is safe to assume it was not evaluated yet
				Self::ensure(config, opts, &name, &pruned)?;
			}
		}
		Ok(())
	}

	fn prune(
		config: &Config,
		name: &str,
		machine: &[String],
		whole_dist: bool,
	) -> Result<Vec<String>> {
		let mut secrets = config.data.secrets.write().expect("not poisoned");
		let Some(dists) = secrets.get_mut(name) else {
			bail!("secret {name} not found");
		};
		let owners_before: BTreeSet<String> = dists
			.owners()
			.filter_map(|o| o.as_host().map(str::to_owned))
			.collect();

		if machine.is_empty() && whole_dist {
			for dist in dists.distributions_mut() {
				dist.prune("manual prune".to_owned());
			}
		} else if machine.is_empty() {
			let dist = dists
				.distributions_mut()
				.exactly_one()
				.map_err(|e| anyhow!("{e}"))
				.context("with no machine specified, there should be exactly one distribution")?;
			dist.prune("manual prune".to_owned());
		} else if whole_dist {
			for dist in dists.distributions_mut() {
				if machine
					.iter()
					.any(|m| dist.owners().any(|o| o.as_host() == Some(m.as_str())))
				{
					dist.prune(format!(
						"manual prune of distribution containing {}",
						machine.join(", ")
					));
				}
			}
		} else {
			let owners: BTreeSet<SecretOwner> = machine.iter().map(SecretOwner::host).collect();
			for dist in dists.distributions_mut() {
				dist.prune_owners(&owners, "manual prune".to_owned());
			}
		}

		let owners_after: BTreeSet<String> = dists
			.owners()
			.filter_map(|o| o.as_host().map(str::to_owned))
			.collect();
		Ok(owners_before.difference(&owners_after).cloned().collect())
	}

	fn ensure(config: &Config, opts: &FleetOpts, name: &str, machine: &[String]) -> Result<()> {
		let hosts: Vec<String> = if machine.is_empty() {
			config
				.list_hosts()?
				.into_iter()
				.filter(|h| opts.should_skip(h).ok() != Some(true))
				.map(|h| h.name)
				.collect()
		} else {
			machine.to_vec()
		};

		for hostname in &hosts {
			let nixos_cfg = config.system_config(hostname)?;
			let secrets = nix_go!(nixos_cfg.secrets);
			if secrets.has_field(name)? {
				info!("ensuring secret {name} for {hostname}");
				// Force evaluation of secret parts, triggering __fleetEnsureHostSecret
				nix_go!(secrets[{ name }].definition.parts);
			}
		}
		Ok(())
	}
}
