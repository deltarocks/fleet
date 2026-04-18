use std::io::{Write as _, stdout};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser;
use fleet_base::{fleetdata::SecretOwner, host::Config, opts::FleetOpts};
use itertools::Itertools as _;
use tracing::warn;

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
