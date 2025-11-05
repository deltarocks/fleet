use std::{
	collections::{BTreeMap, BTreeSet, HashSet},
	io::{self, Read, Write, stdin, stdout},
	path::PathBuf,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use clap::Parser;
use fleet_base::{
	fleetdata::{
		FleetHostSecret, FleetSecretData, FleetSecretPart, FleetSharedSecret, encrypt_secret_data,
	},
	host::Config,
	opts::FleetOpts,
	secret::{Expectations, RegenerationReason, SharedSecretDefinition, secret_needs_regeneration},
};
use fleet_shared::SecretData;
use nix_eval::{NixType, Value, nix_go, nix_go_json};
use owo_colors::OwoColorize;
use serde::Deserialize;
use tabled::{Table, Tabled};
use tokio::{fs::read, task::spawn_blocking};
use tracing::{Instrument, error, info, info_span, warn};

#[derive(Parser)]
pub enum Secret {
	AddManager,
	/// Force load host keys for all defined hosts
	ForceKeys,
	/// Add secret, data should be provided in stdin
	AddShared {
		/// Secret name
		name: String,
		/// Secret owners
		#[clap(long, short)]
		machines: Vec<String>,
		/// Override secret if already present
		#[clap(long)]
		force: bool,
		/// Secret public part
		#[clap(long)]
		public: Option<String>,
		/// Load public part from specified file
		#[clap(long)]
		public_file: Option<PathBuf>,

		/// Create a notification on secret expiration
		#[clap(long)]
		expires_at: Option<DateTime<Utc>>,

		/// Secret with this name already exists, override its value while keeping the same owners.
		#[clap(long)]
		re_add: bool,

		/// How to name public secret part
		#[clap(long, short = 'p', default_value = "public")]
		public_part: String,
		/// How to name private secret part
		#[clap(short = 's', long, default_value = "secret")]
		part: String,
	},
	/// Add secret, data should be provided in stdin
	Add {
		/// Secret name
		name: String,
		/// Secret owner
		#[clap(short = 'm', long)]
		machine: String,
		/// Replace secret if already present
		#[clap(long)]
		replace: bool,
		/// Add new parts to existing secret
		#[clap(long)]
		merge: bool,
		/// Secret public part
		#[clap(long)]
		public: Option<String>,
		/// Load public part from specified file
		#[clap(long)]
		public_file: Option<PathBuf>,

		/// How to name public secret part
		#[clap(short = 'p', long, default_value = "public")]
		public_part: String,
		/// How to name private secret part
		#[clap(short = 's', long, default_value = "secret")]
		part: String,
	},
	/// Read secret from remote host, requires sudo on said host
	Read {
		name: String,
		#[clap(short = 'm', long)]
		machine: String,

		/// Which private secret part to read
		#[clap(short = 'p', long, default_value = "secret")]
		part: String,
	},
	/// Read secret from remote host, requires sudo on said host
	ReadShared {
		name: String,
		/// Which private secret part to read
		#[clap(short = 'p', long, default_value = "secret")]
		part: String,
		/// Which host should we use to decrypt, in case if reencryption is required, without
		/// regeneration
		#[clap(long)]
		prefer_identities: Vec<String>,
	},
	UpdateShared {
		name: String,

		#[clap(short = 'm', long)]
		machine: Option<Vec<String>>,

		#[clap(long)]
		add_machine: Vec<String>,
		#[clap(long)]
		remove_machine: Vec<String>,

		/// Which host should we use to decrypt
		#[clap(long)]
		prefer_identities: Vec<String>,
	},
	Regenerate {
		/// Which host should we use to decrypt, in case if reencryption is required, without
		/// regeneration
		#[clap(long)]
		prefer_identities: Vec<String>,
		/// Only regenerate shared secrets
		#[clap(long)]
		skip_hosts: bool,
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

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(config, secret, definition, prefer_identities))]
async fn maybe_regenerate_shared_secret(
	secret_name: &str,
	config: &Config,
	mut secret: FleetSharedSecret,
	definition: SharedSecretDefinition,
	prefer_identities: &[String],
	expectations: &Expectations,
) -> Result<FleetSharedSecret> {
	let reason = secret_needs_regeneration(&secret.secret, &secret.owners, expectations);
	let value = definition.definition_value();

	let (should_reencrypt, reason) = match reason {
		Some(RegenerationReason::OwnersAdded(_)) => {
			// Secret always needs to be reencrypted for new owners to be able to read it
			(
				true,
				if nix_go_json!(value.regenerateOnOwnerAdded) {
					reason
				} else {
					None
				},
			)
		}
		Some(RegenerationReason::OwnersRemoved(_)) => {
			// No need to reencrypt, we can just leave stanzas in place.
			if nix_go_json!(value.regenerateOnOwnerRemoved) {
				(true, reason)
			} else {
				(false, None)
			}
		}
		Some(_) => (true, reason),
		None => (false, None),
	};

	if let Some(reason) = reason {
		info!("secret needs to be regenerated: {reason}");
		let generated = generate_shared(config, secret_name, definition, expectations).await?;
		Ok(generated)
	} else if should_reencrypt {
		info!("secret needs to be reencrypted");
		let identity_holder = if !prefer_identities.is_empty() {
			prefer_identities
				.iter()
				.find(|i| secret.owners.iter().any(|s| s == *i))
		} else {
			secret.owners.first()
		};
		let Some(identity_holder) = identity_holder else {
			bail!("no available holder found");
		};

		for (part_name, part) in secret.secret.parts.iter_mut() {
			let _span = info_span!("part reencryption", part_name);
			if !part.raw.encrypted {
				continue;
			}
			let host = config.host(identity_holder).await?;
			let encrypted = host
				.reencrypt(
					part.raw.clone(),
					expectations.owners.iter().cloned().collect(),
				)
				.await?;
			part.raw = encrypted;
		}
		secret.owners = expectations.owners.clone();
		Ok(secret)
	} else {
		Ok(secret)
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum GeneratorKind {
	Impure,
	Pure,
}

async fn generate_pure(
	_config: &Config,
	_display_name: &str,
	_secret: Value,
	_default_generator: Value,
	_expectations: &Expectations,
) -> Result<FleetSecretData> {
	bail!("pure generators are broken for now")
}
async fn generate_impure(
	config: &Config,
	_display_name: &str,
	secret: Value,
	default_generator: Value,
	expectations: &Expectations,
) -> Result<FleetSecretData> {
	let generator = nix_go!(secret.generator);
	let on: Option<String> = nix_go_json!(default_generator.impureOn);

	let nixpkgs = &config.nixpkgs;

	let host = if let Some(on) = &on {
		config.host(on).await?
	} else {
		config.local_host()
	};
	let on_pkgs = host.pkgs().await?;
	let mk_secret_generators = nix_go!(on_pkgs.mkSecretGenerators);

	let mut recipients = Vec::new();
	for owner in &expectations.owners {
		let key = config.key(owner).await?;
		recipients.push(key);
	}
	let generators = nix_go!(mk_secret_generators(Obj { recipients }));
	let pkgs_and_generators = on_pkgs.attrs_update(generators)?;

	let call_package = nix_go!(nixpkgs.lib.callPackageWith(pkgs_and_generators));

	let generator = nix_go!(call_package(generator)(Obj {}));

	let generator = spawn_blocking(move || generator.build("out"))
		.await
		.expect("nix build shouldn't fail")?;
	let generator = host.remote_derivation(&generator).await?;

	let out_parent = host.mktemp_dir().await?;
	let out = format!("{out_parent}/out");

	let mut r#gen = host.cmd(generator).await?;
	r#gen.env("out", &out);
	if on.is_none() {
		// This path is local, thus we can feed `OsString` directly to env var... But I don't think that's necessary to handle.
		let project_path: String = config
			.directory
			.clone()
			.into_os_string()
			.into_string()
			.map_err(|s| anyhow!("fleet project path is not utf-8: {s:?}"))?;
		r#gen.env("FLEET_PROJECT", project_path);
	}
	r#gen.run().await.context("impure generator")?;

	{
		let marker = host.read_file_text(format!("{out}/marker")).await?;
		ensure!(marker == "SUCCESS", "generation not succeeded");
	}

	let mut parts = BTreeMap::new();
	for part in host.read_dir(&out).await? {
		if part == "created_at" || part == "expires_at" || part == "marker" {
			continue;
		}
		let contents: SecretData = host
			.read_file_text(format!("{out}/{part}"))
			.await?
			.parse()
			.map_err(|e| anyhow!("failed to decode secret {out:?} part {part:?}: {e}"))?;
		parts.insert(part.to_owned(), FleetSecretPart { raw: contents });
	}

	let created_at = host.read_file_value(format!("{out}/created_at")).await?;
	let expires_at = host.read_file_value(format!("{out}/expires_at")).await.ok();

	let new_data = FleetSecretData {
		created_at,
		expires_at,
		parts,
		generation_data: expectations.generation_data.clone(),
	};

	if let Some(reason) = secret_needs_regeneration(&new_data, &expectations.owners, expectations) {
		bail!("newly generated secret needs to be regenerated: {reason}")
	}

	Ok(new_data)
}

async fn generate(
	config: &Config,
	display_name: &str,
	secret: Value,
	expectations: &Expectations,
) -> Result<FleetSecretData> {
	let generator = nix_go!(secret.generator);
	// Can't properly check on nix module system level
	{
		let gen_ty = generator.type_of();
		if matches!(gen_ty, NixType::Null) {
			bail!("secret has no generator defined, can't automatically generate it.");
		}
		if matches!(gen_ty, NixType::Attrs) {
			if !generator.has_field("__functor")? {
				bail!("generator should be functor, got {gen_ty:?}");
			}
		} else if matches!(gen_ty, NixType::Function) {
			bail!("generator should be functor, got {gen_ty:?}");
		}
	}
	let nixpkgs = &config.nixpkgs;
	let default_pkgs = &config.default_pkgs;
	let default_mk_secret_generators = nix_go!(default_pkgs.mkSecretGenerators);
	// Generators provide additional information in passthru, to access
	// passthru we should call generator, but information about where this generator is supposed to build
	// is located in passthru... Thus evaluating generator on host.
	//
	// Maybe it is also possible to do some magic with __functor?
	//
	// I don't want to make modules always responsible for additional secret data anyway,
	// so it should be in derivation, and not in the secret data itself.
	let generators = nix_go!(default_mk_secret_generators(Obj {
		recipients: <Vec<String>>::new(),
	}));
	let pkgs_and_generators = default_pkgs.clone().attrs_update(generators)?;

	let call_package = nix_go!(nixpkgs.lib.callPackageWith(pkgs_and_generators));
	let default_generator = nix_go!(call_package(generator)(Obj {}));

	let kind: GeneratorKind = nix_go_json!(default_generator.generatorKind);

	match kind {
		GeneratorKind::Impure => {
			generate_impure(
				config,
				display_name,
				secret,
				default_generator,
				expectations,
			)
			.await
		}
		GeneratorKind::Pure => {
			generate_pure(
				config,
				display_name,
				secret,
				default_generator,
				expectations,
			)
			.await
		}
	}
}
async fn generate_shared(
	config: &Config,
	display_name: &str,
	secret: SharedSecretDefinition,
	expectations: &Expectations,
) -> Result<FleetSharedSecret> {
	// let owners: Vec<String> = nix_go_json!(secret.expectedOwners);
	Ok(FleetSharedSecret {
		managed: Some(true),
		secret: generate(
			config,
			display_name,
			secret.definition_value(),
			expectations,
		)
		.await?,
		owners: expectations.owners.clone(),
	})
}

async fn parse_public(
	public: Option<String>,
	public_file: Option<PathBuf>,
) -> Result<Option<SecretData>> {
	Ok(match (public, public_file) {
		(Some(v), None) => Some(SecretData {
			data: v.into(),
			encrypted: false,
		}),
		(None, Some(v)) => Some(SecretData {
			data: read(v).await?,
			encrypted: false,
		}),
		(Some(_), Some(_)) => {
			bail!("only public or public_file should be set")
		}
		(None, None) => None,
	})
}

async fn parse_secret() -> Result<Option<Vec<u8>>> {
	let mut input = vec![];
	stdin().read_to_end(&mut input)?;
	if input.is_empty() {
		Ok(None)
	} else {
		Ok(Some(input))
	}
}

fn parse_machines(
	initial: BTreeSet<String>,
	machines: Option<Vec<String>>,
	mut add_machines: Vec<String>,
	mut remove_machines: Vec<String>,
) -> Result<BTreeSet<String>> {
	if machines.is_none() && add_machines.is_empty() && remove_machines.is_empty() {
		bail!("no operation");
	}

	let initial_machines = initial.clone();
	let mut target_machines = initial;
	info!("Currently encrypted for {initial_machines:?}");

	if let Some(machines) = machines {
		ensure!(
			add_machines.is_empty() && remove_machines.is_empty(),
			"can't combine --machines and --add-machines/--remove-machines"
		);
		let target = initial_machines.iter().collect::<HashSet<_>>();
		let source = machines.iter().collect::<HashSet<_>>();
		for removed in target.difference(&source) {
			remove_machines.push((*removed).clone());
		}
		for added in source.difference(&target) {
			add_machines.push((*added).clone());
		}
	}

	for machine in &remove_machines {
		if !target_machines.remove(machine) {
			warn!("secret is not enabled for {machine}");
		}
	}
	for machine in &add_machines {
		if !target_machines.insert(machine.to_owned()) {
			warn!("secret is already added to {machine}");
		}
	}
	if !remove_machines.is_empty() {
		// TODO: maybe force secret regeneration?
		// Not that useful without revokation.
		warn!(
			"secret will not be regenerated for removed machines, and until host rebuild, they will still possess the ability to decode secret"
		);
	}
	Ok(target_machines)
}
impl Secret {
	pub async fn run(self, config: &Config, opts: &FleetOpts) -> Result<()> {
		match self {
			Secret::AddManager => {
				todo!("part of fleet-pusher")
			}
			Secret::ForceKeys => {
				for host in config.list_hosts().await? {
					if opts.should_skip(&host).await? {
						continue;
					}
					config.key(&host.name).await?;
				}
			}
			Secret::AddShared {
				machines,
				name,
				force,
				public,
				public_part: public_name,
				public_file,
				expires_at,
				re_add,
				part: part_name,
			} => {
				let mut machines: BTreeSet<String> = machines.into_iter().collect();
				// TODO: Forbid updating secrets with set expectedOwners (= not user-managed).

				if let Some(old_shared) = config.shared_secret(&name)? {
					if !force && !re_add {
						bail!("secret already defined");
					};
					if old_shared.managed.unwrap_or(false) {
						bail!("secret is marked as managed, should not be updated manually");
					};
					if re_add {
						// Fixme: use clap to limit this usage
						ensure!(!force, "--force and --readd are not compatible");
						ensure!(
							machines.is_empty(),
							"you can't use machines argument for --readd"
						);
						machines = old_shared.owners;
					}
				} else if re_add {
					bail!("secret doesn't exists");
				};

				let recipients = config
					.recipients(machines.iter().cloned().collect())
					.await?;

				let mut parts = BTreeMap::new();

				let mut input = vec![];
				io::stdin().read_to_end(&mut input)?;

				if !input.is_empty() {
					let encrypted = encrypt_secret_data(recipients.iter(), input)
						.ok_or_else(|| anyhow!("no recipients provided"))?;
					parts.insert(part_name, FleetSecretPart { raw: encrypted });
				}

				if let Some(public) = parse_public(public, public_file).await? {
					parts.insert(public_name, FleetSecretPart { raw: public });
				}

				config.replace_shared(
					name,
					FleetSharedSecret {
						managed: Some(false),
						owners: machines,
						secret: FleetSecretData {
							created_at: Utc::now(),
							expires_at,
							parts,
							generation_data: serde_json::Value::Null,
						},
					},
				);
			}
			Secret::Add {
				machine,
				name,
				replace,
				merge,
				public,
				public_part: public_name,
				public_file,
				part: part_name,
			} => {
				if config.has_secret(&machine, &name) && !replace && !merge {
					bail!(
						"secret already defined.\nUse --replace to override, or --merge to add new parts to existing secret"
					);
				}

				let mut out = if merge && !replace {
					config
						.host_secret(&machine, &name)
						.context("failed to read existing secret for --merge")?
				} else {
					FleetHostSecret {
						managed: Some(false),
						secret: FleetSecretData {
							created_at: Utc::now(),
							expires_at: None,
							parts: BTreeMap::new(),
							generation_data: serde_json::Value::Null,
						},
					}
				};
				if out.managed.unwrap_or(false) {
					bail!("secret is managed by fleet and should not be updated manually");
				}
				out.managed = Some(false);

				if let Some(secret) = parse_secret().await? {
					let recipient = config.recipient(&machine).await?;
					let encrypted =
						encrypt_secret_data([&recipient], secret).expect("recipient provided");
					if out
						.secret
						.parts
						.insert(part_name.clone(), FleetSecretPart { raw: encrypted })
						.is_some() && !replace
					{
						bail!(
							"part {part_name:?} is already defined, use --replace if you wish to replace it"
						);
					}
				}

				if let Some(public) = parse_public(public, public_file).await? {
					if out
						.secret
						.parts
						.insert(public_name.clone(), FleetSecretPart { raw: public })
						.is_some() && !replace
					{
						bail!(
							"part {public_name:?} is already defined, use --replace if you wish to replace it"
						);
					}
				};

				config.insert_secret(&machine, name, out);
			}
			#[allow(clippy::await_holding_refcell_ref)]
			Secret::Read {
				name,
				machine,
				part: part_name,
			} => {
				let secret = config.host_secret(&machine, &name)?;
				let Some(secret) = secret.secret.parts.get(&part_name) else {
					bail!("no part {part_name} in secret {name}");
				};
				let data = if secret.raw.encrypted {
					let host = config.host(&machine).await?;
					host.decrypt(secret.raw.clone()).await?
				} else {
					secret.raw.data.clone()
				};

				stdout().write_all(&data)?;
			}
			Secret::ReadShared {
				name,
				part: part_name,
				prefer_identities,
			} => {
				let Some(secret) = config.shared_secret(&name)? else {
					bail!("secret doesn't exists");
				};
				let Some(part) = secret.secret.parts.get(&part_name) else {
					bail!("no part {part_name} in secret {name}");
				};
				let data = if part.raw.encrypted {
					let identity_holder = if !prefer_identities.is_empty() {
						prefer_identities
							.iter()
							.find(|i| secret.owners.iter().any(|s| s == *i))
					} else {
						secret.owners.first()
					};
					let Some(identity_holder) = identity_holder else {
						bail!("no available holder found");
					};
					let host = config.host(identity_holder).await?;
					host.decrypt(part.raw.clone()).await?
				} else {
					part.raw.data.clone()
				};
				stdout().write_all(&data)?;
			}
			Secret::UpdateShared {
				name,
				machine,
				add_machine,
				remove_machine,
				prefer_identities,
			} => {
				// TODO: Forbid updating secrets with set expectedOwners (= not user-managed).

				let Some(secret) = config.shared_secret(&name)? else {
					bail!("secret doesn't exists");
				};
				if secret.secret.parts.values().all(|v| !v.raw.encrypted) {
					bail!("no secret");
				}

				let initial_machines = secret.owners.clone();
				let target_machines = parse_machines(
					initial_machines.clone(),
					machine,
					add_machine,
					remove_machine,
				)?;

				if target_machines.is_empty() {
					info!("no machines left for secret, removing it");
					config.remove_shared(&name);
					return Ok(());
				}

				let definition = config.shared_secret_definition(&name)?;
				let expectations = definition
					.expectations()
					.with_context(|| format!("expectations for shared {name:?}"))?;

				let updated = maybe_regenerate_shared_secret(
					&name,
					config,
					secret,
					definition,
					&prefer_identities,
					&expectations,
				)
				.await?;
				config.replace_shared(name, updated);
			}
			Secret::Regenerate {
				prefer_identities,
				skip_hosts,
			} => {
				info!("checking for secrets to regenerate");
				let expected_shared_set = config
					.list_configured_shared()
					.await?
					.into_iter()
					.collect::<HashSet<_>>();
				let stored_shared_set = config.list_shared().into_iter().collect::<HashSet<_>>();
				{
					// Generate missing shared
					let _span = info_span!("shared").entered();
					for missing in expected_shared_set.difference(&stored_shared_set) {
						let definition = config.shared_secret_definition(missing)?;
						if !definition.is_managed()? {
							info!("skipping unmanaged secret: {missing}");
							continue;
						}
						let expectations = definition
							.expectations()
							.with_context(|| format!("expectations for shared {missing:?}"))?;
						info!("generating secret: {missing}");
						let shared = generate_shared(config, missing, definition, &expectations)
							.in_current_span()
							.await?;
						config.replace_shared(missing.to_string(), shared)
					}
				}
				if !skip_hosts {
					for host in config.list_hosts().await? {
						if opts.should_skip(&host).await? {
							continue;
						}

						let _span = info_span!("host", host = host.name).entered();
						let expected_set = host
							.list_defined_secrets()?
							.into_iter()
							.collect::<HashSet<_>>();
						let stored_set = config
							.list_secrets(&host.name)
							.into_iter()
							.collect::<HashSet<_>>();
						for missing_secret in expected_set.difference(&stored_set) {
							let secret = host.secret_definition(missing_secret)?;
							if secret.is_shared()? {
								continue;
							}
							info!("generating missing secret: {missing_secret}");
							let expectations = secret.expectations().with_context(|| {
								format!("expectations for {missing_secret:?} of {:?}", host.name)
							})?;
							let generated = match generate(
								config,
								missing_secret,
								secret.definition_value()?,
								&expectations,
							)
							.in_current_span()
							.await
							{
								Ok(v) => v,
								Err(e) => {
									error!("{e:?}");
									continue;
								}
							};
							config.insert_secret(
								&host.name,
								missing_secret.to_string(),
								FleetHostSecret {
									managed: Some(true),
									secret: generated,
								},
							)
						}
						for known_secret in stored_set.intersection(&expected_set) {
							let secret = host.secret_definition(known_secret)?;
							if secret.is_shared()? {
								continue;
							}
							info!("updating secret: {known_secret}");
							let data = config.host_secret(&host.name, known_secret)?;
							let expectations = secret.expectations()?;
							if let Some(regen_reason) = data.needs_regeneration(&expectations) {
								info!("needs regeneration: {regen_reason}");
								let generated = match generate(
									config,
									known_secret,
									secret.definition_value()?,
									&expectations,
								)
								.in_current_span()
								.await
								{
									Ok(v) => v,
									Err(e) => {
										error!("{e:?}");
										continue;
									}
								};
								config.insert_secret(
									&host.name,
									known_secret.to_string(),
									FleetHostSecret {
										managed: Some(true),
										secret: generated,
									},
								)
							}
						}
						for removed_secret in stored_set.difference(&expected_set) {
							let definition = host.secret_definition(removed_secret)?;
							if definition.is_shared()? {
								continue;
							}
							info!("removing secret: {removed_secret}");
							config.remove_secret(&host.name, removed_secret);
						}
					}
				}
				for known_secret in stored_shared_set.intersection(&expected_shared_set) {
					info!("updating shared secret: {known_secret}");
					let data = config.shared_secret(known_secret)?.expect("exists");

					let definition = config.shared_secret_definition(known_secret)?;
					let expectations = definition.expectations()?;
					config.replace_shared(
						known_secret.to_owned(),
						maybe_regenerate_shared_secret(
							known_secret,
							config,
							data,
							definition,
							&prefer_identities,
							&expectations,
						)
						.await?,
					);
				}
				for removed_secret in stored_shared_set.difference(&expected_shared_set) {
					info!("removing shared secret: {removed_secret}");
					config.remove_shared(removed_secret);
				}
			}
			Secret::List {} => {
				let _span = info_span!("loading secrets").entered();
				let configured = config.list_configured_shared().await?;
				#[derive(Tabled)]
				struct SecretDisplay {
					#[tabled(rename = "Name")]
					name: String,
					#[tabled(rename = "Owners")]
					owners: String,
				}
				let mut table = vec![];
				for name in configured.iter().cloned() {
					let config = config.clone();
					let data = config.shared_secret(&name)?.expect("exists");
					let definition = config.shared_secret_definition(&name)?;
					let expectations = definition.expectations()?;
					let owners = data
						.owners
						.iter()
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
				}
				info!("loaded\n{}", Table::new(table).to_string())
			}
			Secret::Edit {
				name,
				machine,
				part,
				add,
			} => {
				let secret = config.host_secret(&machine, &name)?;
				if let Some(data) = secret.secret.parts.get(&part) {
					let host = config.host(&machine).await?;
					let secret = host.decrypt(data.raw.clone()).await?;
					String::from_utf8(secret).context("secret is not utf8")?
				} else if add {
					String::new()
				} else {
					bail!("part {part} not found in secret {name}. Did you mean to `--add` it?");
				};
			}
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
