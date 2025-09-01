use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use clap::Parser;
use fleet_base::host::Config;
use nix_eval::nix_go_json;

#[derive(Parser)]
pub struct Info {
	#[clap(long)]
	json: bool,
	#[clap(subcommand)]
	cmd: InfoCmd,
}

#[derive(Parser)]
pub enum InfoCmd {
	/// List hosts
	ListHosts {
		#[clap(long)]
		tagged: Vec<String>,
	},
	/// List ips
	HostIps {
		host: String,
		#[clap(long)]
		external: bool,
		#[clap(long)]
		internal: bool,
	},
}

impl Info {
	pub async fn run(self, config: &Config) -> Result<()> {
		let mut data = Vec::new();
		match self.cmd {
			InfoCmd::ListHosts { ref tagged } => {
				'host: for host in config.list_hosts().await? {
					if !tagged.is_empty() {
						let config = &config.config_field;
						let host_name = &host.name;
						let tags: Vec<String> = nix_go_json!(config.hosts[host_name].tags);
						for tag in tagged {
							if !tags.contains(tag) {
								continue 'host;
							}
						}
					}
					data.push(host.name);
				}
			}
			InfoCmd::HostIps {
				host,
				external,
				internal,
			} => {
				ensure!(
					external || internal,
					"at leas one of --external or --internal must be set"
				);
				let mut out = <BTreeSet<String>>::new();
				let host = config.system_config(&host).await?;
				if external {
					let data: Vec<String> = nix_go_json!(host.network.externalIps);
					out.extend(data);
				}
				if internal {
					let data: Vec<String> = nix_go_json!(host.network.internalIps);
					out.extend(data);
				}
				for ip in out {
					data.push(ip);
				}
			}
		}

		if self.json {
			let v = serde_json::to_string_pretty(&data)?;
			print!("{}", v);
		} else {
			for v in data {
				println!("{}", v);
			}
		}
		Ok(())
	}
}
