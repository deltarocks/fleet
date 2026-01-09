use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use anyhow::{Context, bail, ensure};
use fleet_shared::SecretData;
use itertools::Itertools;
use nix_eval::{NativeFn, Value, nix_go, nix_go_json};
use serde::Deserialize;
use tracing::{info, warn};

use crate::fleetdata::{
	Expectations, FleetSecretData, FleetSecretDistribution, FleetSecretPart, GeneratorPart,
};
use crate::host::{Config, ConfigHost};
use crate::secret::{RegenerationReason, secret_needs_regeneration};
use anyhow::{Result, anyhow};

#[derive(thiserror::Error, Debug)]
enum Error {}

pub static PRIMOPS_DATA: OnceLock<Config> = OnceLock::new();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum GeneratorKind {
	Impure,
	Pure,
}

pub fn get_pkgs_and_generators(host_on: &ConfigHost, recipients: Vec<String>) -> Result<Value> {
	info!("get pkgs");
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
			let pkgs_and_generators =
				get_pkgs_and_generators(&host_on, expectations.owners.iter().cloned().collect())
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

			let new_data = FleetSecretDistribution {
				secret: new_data,
				owners: expectations.owners.clone(),
				_deprecated_managed: true,
			};

			if let Some(reason) = secret_needs_regeneration(&new_data, &expectations) {
				bail!("newly generated secret needs to be regenerated: {reason}")
			}

			Ok(new_data)
		}
		GeneratorKind::Pure => {
			bail!("pure generators are disabled for now")
		}
	}
}

pub fn init_primops() {
	info!("initializing primops");
	NativeFn::new(
		c"__fleetEnsureHostSecret",
		c"Ensure secret existence for a host, regenerating it in case of some mismatch",
		[c"host", c"secret", c"generator"],
		|es, [host, secret, generator]| {
			info!("get host");
			let host = host.to_string()?;
			info!("get secret");
			let secret = secret.to_string()?;

			info!("get config");
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

				(true, shared_def.generator()?, expected_owners)
			} else {
				if shared_def.is_some() {
					bail!("hosts can only have their own generators for non-shared secrets, either set host secret generator to \"shared\", or remove shared secret generator at fleetConfiguration.secrets.{secret}.generator")
				}

				(false, generator.clone(), BTreeSet::from_iter([host.clone()]))
			};

			let default_generator_drv = get_default_generator_drv(config, &generator).context("failed to evaluate default generator")?;
			let expectations = Expectations {
				parts: nix_go_json!(default_generator_drv.parts),
				generation_data: nix_go_json!(default_generator_drv.generationData),
				owners: expected_owners,
			};

			let reason: RegenerationReason = 'regenerate: {
				let Some(existing) = config
					.host_secret(&host, &secret) else {
					break 'regenerate RegenerationReason::Missing;
				};
				if let Some(reason) = secret_needs_regeneration(&existing, &expectations) {
					break 'regenerate reason;
				}

				let mut parts = expectations.parts.clone();

				let mut out = HashMap::new();
				for (part_name, part) in &existing.secret.parts {
					let Some(definition) = parts.remove(part_name) else {
						warn!("secret {secret} part {part_name} is stored, but not defined in nixos config, it will not be passed to nix");
						continue;
					};
					assert!(definition.encrypted != part.raw.encrypted, "encryption status is checked by secret_needs_regeneration");
					out.insert(part_name.as_str(), Value::new_attrs(HashMap::from_iter([("raw", Value::new_str(&part.raw.to_string()))])));
				}
				assert!(parts.is_empty(), "secret part is missing, secret_needs_regeneration should check that");

				return Ok(Value::new_attrs(out))
			};

			todo!()


		},
	)
	.register();
}
