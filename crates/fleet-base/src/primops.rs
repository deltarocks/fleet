use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, bail};
use itertools::Itertools;
use nix_eval::{NativeFn, Value, nix_go, nix_go_json};
use serde::Deserialize;
use tracing::{info, warn};

use crate::fleetdata::{FleetData, FleetSecrets};
use crate::host::Config;

#[derive(thiserror::Error, Debug)]
enum Error {}

struct Parts {
	encrypted: Vec<String>,
	public: Vec<String>,
}

trait SecretsBackend {
	fn has_shared(&self, name: &str);
	fn has_host(&self, host: &str, name: &str);
	fn shared_parts(&self, name: &str) -> Parts;
	fn host_parts(&self, host: &str, name: &str) -> Parts;
}

struct FsSecretsBackend {}

pub static PRIMOPS_DATA: OnceLock<Config> = OnceLock::new();

#[derive(Deserialize, Debug)]
struct GeneratorPart {
	encrypted: bool,
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

			info!("get pkgs");
			let nixpkgs = &config.nixpkgs;
			let default_pkgs = &config.default_pkgs;
			let default_mk_secret_generators = nix_go!(default_pkgs.mkSecretGenerators);
			let generators = nix_go!(default_mk_secret_generators(Obj {
				recipients: <Vec<String>>::new(),
			}));
			let pkgs_and_generators = default_pkgs.clone().attrs_update(generators)?;

			info!("call package");
			let call_package = nix_go!(nixpkgs.lib.callPackageWith(pkgs_and_generators));
			let default_generator = call_package
				.call(generator.clone())
				.context("calling callPackage with generator")?
				.call(Value::new_attrs(HashMap::new()))
				.context("providing extra callPackage args")?;

			info!("get parts");
			let mut parts: BTreeMap<String, GeneratorPart> = nix_go_json!(default_generator.parts);
			info!("got parts: {parts:?}");

			let Some(existing) = config
				.host_secret(&host, &secret) else {
				bail!("missing secret {secret} for host {host}; secret needs regeneration")
			};

			info!("got existing: {existing:?}");

			let mut out = HashMap::new();

			for (part_name, part) in &existing.secret.parts {
				let Some(definition) = parts.remove(part_name) else {
					warn!("secret {secret} part {part_name} is stored, but not defined in nixos config, it will not be passed to nix");
					continue;
				};
				if definition.encrypted != part.raw.encrypted {
					bail!("secret {secret} part {part_name} is supposed to be {}, but it is {}; secret needs regeneration", if definition.encrypted {"encrypted"} else {"unencrypted"}, if part.raw.encrypted {"encrypted"} else {"unencrypted"});
				}
				out.insert(part_name.as_str(), Value::new_attrs(HashMap::from_iter([("raw", Value::new_str(&part.raw.to_string()))])));
			}
			if !parts.is_empty(){
				let defs = parts.keys().collect_vec();
				bail!("secret parts are defined, but not stored: {defs:?}, secret needs regeneration")
			}

			Ok(Value::new_attrs(out))
		},
	)
	.register();
}
