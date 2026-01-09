use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use anyhow::{Context, bail, ensure};
use fleet_shared::SecretData;
use itertools::Itertools;
use nix_eval::{NativeFn, Value, await_in_nix, nix_go, nix_go_json};
use serde::Deserialize;
use tracing::{info, warn};

use crate::fleetdata::{
	Expectations, FleetSecretData, FleetSecretDistribution, FleetSecretPart, GeneratorPart,
	RegenerationConstraints, SecretOwner,
};
use crate::host::{Config, ConfigHost};
use anyhow::{Result, anyhow};

pub static PRIMOPS_DATA: OnceLock<Config> = OnceLock::new();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum GeneratorKind {
	Impure,
	Pure,
}

pub fn get_pkgs_and_generators(host_on: &ConfigHost, recipients: Vec<String>) -> Result<Value> {
	let pkgs = host_on.pkgs()?;
	let default_mk_secret_generators = nix_go!(pkgs.mkSecretGenerators);
	let generators = nix_go!(default_mk_secret_generators(Obj { recipients }));
	Ok(pkgs.clone().attrs_update(generators)?)
}
pub fn get_default_pkgs_and_generators(config: &Config) -> Result<Value> {
	let host_on = config.local_host();
	get_pkgs_and_generators(&host_on, vec![])
}
pub fn call_package(config: &Config, pkgs: &Value, package: &Value) -> Result<Value> {
	ensure!(
		package.is_function(),
		"package should be a function to be called with callPackage"
	);
	// No need to use nixpkgs.buildUsing, as only nixpkgs-lib is used.
	let nixpkgs = &config.nixpkgs;
	let call_package = nix_go!(nixpkgs.lib.callPackageWith(pkgs));
	Ok(nix_go!(call_package(package)(Obj {})))
}

pub fn get_default_generator_drv(config: &Config, generator: &Value) -> Result<Value> {
	let default_pkgs_and_generators = get_default_pkgs_and_generators(config)?;
	let default_generator_drv = call_package(config, &default_pkgs_and_generators, generator)
		.context("failed to initialize generator to get metadata")?;

	Ok(default_generator_drv)
}

fn secret_to_parts(
	secret_name: &str,
	secret: &BTreeMap<String, FleetSecretPart>,
	expected: &BTreeMap<String, GeneratorPart>,
) -> Value {
	let mut out = HashMap::new();
	for (part_name, part) in secret {
		if !expected.contains_key(part_name) {
			warn!(
				"secret {secret_name} part {part_name} is stored, but not defined in nixos config, it will not be passed to nix"
			);
			continue;
		};
		out.insert(
			part_name.as_str(),
			Value::new_attrs(HashMap::from_iter([(
				"raw",
				Value::new_str(&part.raw.to_string()),
			)])),
		);
	}

	Value::new_attrs(out)
}

pub async fn generate(
	config: &Config,
	expectations: Expectations,
	generator: &Value,
	default_generator_drv: &Value,
) -> Result<FleetSecretDistribution> {
	let kind: GeneratorKind = nix_go_json!(default_generator_drv.generatorKind);

	match kind {
		GeneratorKind::Impure => {
			let impure_on: Option<String> = nix_go_json!(default_generator_drv.impureOn);

			let host_on = if let Some(on) = &impure_on {
				config
					.host(on)
					.context("failed to get secret generation target host")?
			} else {
				config.local_host()
			};
			let mut recipients = Vec::new();
			for owner in &expectations.owners {
				recipients.push(config.key(owner).await?);
			}
			let pkgs_and_generators = get_pkgs_and_generators(&host_on, recipients)
				.context("failed to get pkgs for target host")?;
			let generator = call_package(config, &pkgs_and_generators, generator)
				.context("failed to evaluate generator for target host")?;

			let generator = generator
				.build("out")
				.context("failed to build generator for target host")?;

			let generator = host_on
				.remote_derivation(&generator)
				.await
				.context("failed to copy generator to target host")?;

			// TODO: Remove destdir after everything is done
			let out_parent = host_on
				.mktemp_dir()
				.await
				.context("failed to prepare generator output dir on target host")?;
			let out = format!("{out_parent}/out");
			let mut generator_cmd = host_on.cmd(generator).await?;
			generator_cmd.env("out", &out);
			if impure_on.is_none() {
				let project_path: String = config
					.directory
					.clone()
					.into_os_string()
					.into_string()
					.map_err(|e| anyhow!("fleet project path is not utf-8: {e:?}"))?;
				generator_cmd.env("FLEET_PROJECT", project_path);
			};
			generator_cmd
				.run()
				.await
				.context("failed to run impure generator")?;

			{
				let marker = host_on.read_file_text(format!("{out}/marker")).await?;
				ensure!(
					marker == "SUCCESS",
					"impure generator ended prematurely, secret generation failed"
				);
			}

			let mut parts = BTreeMap::new();
			for part in host_on.read_dir(&out).await? {
				if part == "created_at" || part == "expires_at" || part == "marker" {
					continue;
				}
				let contents: SecretData = host_on
					.read_file_text(format!("{out}/{part}"))
					.await?
					.parse()
					.map_err(|e| anyhow!("failed to decode secret {out:?} part {part:?}: {e}"))?;
				parts.insert(part.to_owned(), FleetSecretPart { raw: contents });
			}

			let created_at = host_on.read_file_value(format!("{out}/created_at")).await?;
			let expires_at = host_on
				.read_file_value(format!("{out}/expires_at"))
				.await
				.ok();

			let new_data = FleetSecretData {
				created_at,
				expires_at,
				parts,
				generation_data: expectations.generation_data.clone(),
			};

			let new_data =
				FleetSecretDistribution::new(expectations.owners.clone(), new_data, config.now);

			Ok(new_data)
		}
		GeneratorKind::Pure => {
			bail!("pure generators are disabled for now")
		}
	}
}

pub fn init_primops() {
	NativeFn::new(
		c"__fleetEnsureHostSecrets",
		c"Ensure no extra secrets are stored for the host, pruning unknown",
		[c"host", c"expectedNonshared", c"expectedShared", c"rest"],
		|_es, [host, expected_nonshared, expected_shared, rest]| {
			let host = SecretOwner::host(host.to_string()?);
			let expected_nonshared: BTreeSet<String> = expected_nonshared.as_json()?;
			let expected_shared: BTreeSet<String> = expected_shared.as_json()?;

			let mut expected = expected_nonshared;
			expected.extend(expected_shared);

			let config = PRIMOPS_DATA
				.get()
				.expect("primops data should be set on init");

			config
				.data
				.secrets
				.write()
				.expect("no poisoning")
				.prune_host(&host, expected);

			Ok(rest.clone())
		},
	)
	.register();
	NativeFn::new(
		c"__fleetEnsureHostSecret",
		c"Ensure secret existence for a host, regenerating it in case of some mismatch",
		[c"host", c"secret", c"generator"],
		|es, [host, secret, generator]| {
			let host = SecretOwner::host(&host.to_string()?);
			let secret = secret.to_string()?;

			let config = PRIMOPS_DATA
				.get()
				.expect("primops data should be set on init");

			let shared_def = config.secret_definition(&secret).context("failed to get shared secret definition")?;

			let (shared, generator, expected_owners) = if generator.is_string() {
				assert_eq!(generator.to_string()?, "shared", "asserted by nixos type system");
				let Some(shared_def) = shared_def else {
					bail!("secret {secret} is defined on host {host} as shared, but there is no shared secret with same name defined at fleetConfiguration.secrets.{secret}.generator")
				};
				let expected_owners = shared_def.expected_owners()?;

				ensure!(expected_owners.contains(&host), "secret {secret} does not define {host} as expected owner");

				(Some(shared_def.clone()), shared_def.generator()?, expected_owners)
			} else {
				if shared_def.is_some() {
					bail!("hosts can only have their own generators for non-shared secrets, either set host secret generator to \"shared\", or remove shared secret generator at fleetConfiguration.secrets.{secret}.generator")
				}

				(None, generator.clone(), BTreeSet::from_iter([host.clone()]))
			};

			let default_generator_drv = get_default_generator_drv(config, &generator)?;
			let mut expectations = Expectations {
				parts: nix_go_json!(default_generator_drv.parts),
				generation_data: nix_go_json!(default_generator_drv.generationData),
				owners: expected_owners.clone(),
			};
			let constraints = if let Some(shared) = &shared{
				RegenerationConstraints {
					allow_different: nix_go_json!(default_generator_drv.allowDifferent) && shared.allow_different()?,
					regenerate_on_owner_added: shared.regenerate_on_owner_added()?,
					regenerate_on_owner_removed: shared.regenerate_on_owner_added()?,
				}
			} else {
				RegenerationConstraints::host_personal()
			};

			let mut secrets = config.data.secrets.write().expect("no poisoning");
			let dists = secrets.get_or_create(&secret);

				if shared.is_some() {
					dists.prune_shared(&expected_owners, !constraints.allow_different, &expectations.parts, &expectations.generation_data, constraints.regenerate_on_owner_removed, constraints.regenerate_on_owner_added, &config.prefer_identities, config.now);
				} else {
					dists.prune_host(host.clone(), &expectations.parts, &expectations.generation_data, config.now);
				};

				if let Some(dist) = dists.get(&host) {
					return Ok(secret_to_parts(&secret, &dist.secret.parts, &expectations.parts));
				};

				let mut reencrypt_targets = expectations.owners.clone();
				for dist in dists.distributions() {
					for own in dist.owners() {
						reencrypt_targets.remove(own);
					}
				}
				if !constraints.regenerate_on_owner_added {
					if let Some(unpruned) = dists.try_unprune(host.clone()) {
						return Ok(secret_to_parts(&secret, &unpruned.secret.parts, &expectations.parts));
					} else if let Some(best) = dists.best_distribution_for_reencryption(&config.prefer_identities) {
						let new_owners = reencrypt_targets.clone();
						let mut reencrypt_targets = reencrypt_targets;
						reencrypt_targets.extend(best.owners().cloned());

						let mut preferred = best.owners().collect_vec();
						preferred.sort_by_key(|v| !config.prefer_identities.contains(*v));

						warn!("reencrypting secret {secret} as it is missing for host {host}");

						for owner in preferred {
							if let Some(hostname) = owner.as_host() && let Ok(host) = config.host(hostname) {
								let best = best.clone();
								let reencrypt_targets = reencrypt_targets.clone();
								let reencrypted = match await_in_nix(async move {
										host.reencrypt_distribution(&best, reencrypt_targets.clone(), config.now).await
								}) {
									Ok(r) => r,
									Err(e) => {
										warn!("reencryption failed on {hostname}: {e:?}");
										continue;
									}
								};
								dists.extend(reencrypted.clone(), format!("secret was reencrypted to extend with new owners: {new_owners:?}"));
								return Ok(secret_to_parts(&secret, &reencrypted.secret.parts, &expectations.parts));
							};
						}
						warn!("failed to reencrypt using any host")
					};
				};

			if constraints.allow_different {
				for dist in dists.distributions() {
					for own in dist.owners() {
						expectations.owners.remove(own);
					}
				}
			}
			info!("secret {secret} is being generated for {:?}", expectations.owners);

			let expectations_ = expectations.clone();
			let generated = await_in_nix(async move {
				generate(config, expectations_, &generator, &default_generator_drv).await
			})?;

			dists.extend(generated.clone(), format!("secret was generated"));

			return Ok(secret_to_parts(&secret, &generated.secret.parts, &expectations.parts));
		},
	)
	.register();
}
