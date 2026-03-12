use std::{
	collections::{BTreeSet, HashSet},
	io::{Read, stdin},
	path::PathBuf,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use clap::Parser;
use fleet_base::{fleetdata::SecretOwner, host::Config, opts::FleetOpts};
use fleet_shared::SecretData;
use itertools::{ExactlyOneError, Itertools as _};
use tokio::fs::read;
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
		#[clap(short = 'p', long, default_value = "secret")]
		part: Option<String>,

		/// Which host should we use to decrypt, in case if reencryption is required, without
		/// regeneration
		#[clap(long)]
		prefer_identities: Vec<String>,
	},
	/// Prune (remove, mark for regeneration) secrets
	Prune {
		/// Secret to prune
		name: String,

		/// Machines to prune - if specified, only the choosen machines will be pruned
		#[clap(short = 'm', long)]
		machine: Vec<String>,
	},
	/// Ensure secret is generated and not expired
	Ensure {
		/// Secret to ensure generated
		name: String,

		/// Machines to force secret for
		#[clap(short = 'm', long)]
		machine: Vec<String>,
	},
	List {},
	Edit {
		name: String,
		#[clap(short = 'm', long)]
		machine: String,

		#[clap(long)]
		add: bool,

		/// Which private secret part to read
		#[clap(short = 'p', long, default_value = "secret")]
		part: String,
	},
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
				mut prefer_identities,
			} => {
				let secret = config.data.secrets.read().expect("not poisoned");

				let Some(dist) = secret.get("name") else {
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

				let part_name = part_name.unwrap_or_else(|| "secret".to_string());
				let Some(part) = dist.secret.parts.get(&part_name) else {
					bail!("secret part {part_name:?} is not defined");
				};

				// dist.get(SecretOwner(name));

				todo!();
				/*
				let Some(secret) = config.shared_secret(&name) else {
					bail!("secret doesn't exists");
				};

				let dist = if secret.len() == 1 {
					&secret[0]
				} else if let Some(machine) = machine {
					let dist = secret.get(&machine);
					let Some(dist) = dist else {
						bail!("machine {machine} has no distribution of secret {name}");
					};
					prefer_identities.push(machine);
					dist
				} else {
					bail!(
						"secret {name} has shares, but no --machine specified for specifing which do you need"
					)
				};

				let Some(part) = dist.secret.parts.get(&part_name) else {
					bail!("no part {part_name} in secret {name}");
				};
				let data = if part.raw.encrypted {
					let identity_holder = if !prefer_identities.is_empty() {
						prefer_identities
							.iter()
							.find(|i| dist.owners.iter().any(|s| s == *i))
					} else {
						dist.owners.first()
					};
					let Some(identity_holder) = identity_holder else {
						bail!("no available holder found");
					};
					let host = config.host(identity_holder)?;
					host.decrypt(part.raw.clone()).await?
				} else {
					part.raw.data.clone()
				};
				stdout().write_all(&data)?;
				*/
				todo!()
			}
			Secret::List {} => {
				/*
				let _span = info_span!("loading secrets").entered();
				let configured = config.list_configured_shared()?;
				#[derive(Tabled)]
				struct SecretDisplay {
					#[tabled(rename = "Name")]
					name: String,
					#[tabled(rename = "Owners")]
					owners: String,
				}
				// let mut table = vec![];
				for name in configured.iter().cloned() {
					let config = config.clone();
					let data = config.shared_secret(&name).expect("exists");
					/*
										let definition = config.shared_secret_definition(&name)?;
										let expectations = definition.expectations()?;
										let owners = data
											.owners()
											.map(|o| {
												if expectations.owners.contains(o) {
													o.green().to_string()
												} else {
													o.red().to_string()
												}
											})
											.collect::<Vec<_>>();
										table.push(SecretDisplay {
											owners: owners.join(", "),
											name,
										})
					*/
				}
				// info!("loaded\n{}", Table::new(table).to_string())
				*/
				todo!()
			}
			Secret::Edit {
				name,
				machine,
				part,
				add,
			} => {
				/*let secret = config
					.host_secret(&machine, &name)
					.context("secret not found")?;
				if let Some(data) = secret.secret.parts.get(&part) {
					let host = config.host(&machine)?;
					let secret = host.decrypt(data.raw.clone()).await?;
					String::from_utf8(secret).context("secret is not utf8")?
				} else if add {
					String::new()
				} else {
					bail!("part {part} not found in secret {name}. Did you mean to `--add` it?");
				};*/
				todo!()
			}
			Secret::Prune { name, machine } => todo!(),
			Secret::Ensure { name, machine } => todo!(),
		}
		Ok(())
	}
}

/*
async fn edit_temp_file(
	builder: tempfile::Builder<'_, '_>,
	r: Vec<u8>,
	header: &str,
	comment: &str,
) -> Result<(Vec<u8>, Option<String>), anyhow::Error> {
	if !stdin().is_tty() {
		// TODO: Also try to open /dev/tty directly?
		bail!("stdin is not tty, can't open editor");
	}

	use std::fmt::Write;
	let mut file = builder.tempfile()?;

	let mut full_header = String::new();
	let mut had = false;
	for line in header.trim_end().lines() {
		had = true;
		writeln!(&mut full_header, "{comment}{line}")?;
	}
	if had {
		writeln!(&mut full_header, "{}", comment.trim_end())?;
	}
	writeln!(
		&mut full_header,
		"{comment}Do not touch this header! It will be removed automatically"
	)?;

	file.write_all(full_header.as_bytes())?;
	file.write_all(&r)?;

	let abs_path = file.into_temp_path();
	let editor = std::env::var_os("VISUAL")
		.or_else(|| std::env::var_os("EDITOR"))
		.unwrap_or_else(|| "vi".into());
	let editor_args = shlex::bytes::split(editor.as_encoded_bytes())
		.ok_or_else(|| anyhow!("EDITOR env var has wrong syntax"))?;
	let editor_args = editor_args
		.into_iter()
		.map(|v| {
			// Only ASCII subsequences are replaced
			unsafe { OsString::from_encoded_bytes_unchecked(v) }
		})
		.collect_vec();
	let Some((editor, args)) = editor_args.split_first() else {
		bail!("EDITOR env var has no command");
	};
	let mut command = Command::new(editor);
	command.args(args);

	let path_arg = abs_path.canonicalize()?;

	// TODO: Save full state, using tcget/_getmode/_setmode
	let was_raw = terminal::is_raw_mode_enabled()?;
	terminal::enable_raw_mode()?;

	let status = command.arg(path_arg).status().await;

	if !was_raw {
		terminal::disable_raw_mode()?;
	}

	let success = match status {
		Ok(s) => s.success(),
		Err(e) if e.kind() == io::ErrorKind::NotFound => {
			bail!("editor not found")
		}
		Err(e) => bail!("editor spawn error: {e}"),
	};

	let mut file = std::fs::read(&abs_path).context("read editor output")?;
	let Some(v) = file.strip_prefix(full_header.as_bytes()) else {
		todo!();
	};
	todo!();

	// Ok((success, abs_path))
}
*/
