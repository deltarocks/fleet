use std::{
	collections::{BTreeMap, BTreeSet, HashSet},
	future::Future,
	io::Write,
	ops::Deref,
	path::PathBuf,
	pin::Pin,
	str::FromStr,
	sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
use fleet_shared::SecretData;
use nix_eval::{Store, Value, eval_store, nix_go, nix_go_json, util::assert_warn};
use remowt_client::{AgentBundle, Remowt};
use remowt_endpoints::fs::FsClient;
use remowt_link_shared::Address;
use remowt_ui_prompt::auto::AutoPrompter;
use remowt_ui_prompt::bifrost::PromptEndpoints;
use remowt_ui_prompt::{PrependSourcePrompter, Source};
use tabled::Tabled;
use tempfile::NamedTempFile;
use time::UtcDateTime;
use tokio::task::spawn_blocking;
use tracing::warn;

use crate::fleetdata::{
	FleetData, FleetSecretData, FleetSecretDistribution, FleetSecretPart, SecretOwner,
};

pub struct FleetConfigInternals {
	pub prefer_identities: BTreeSet<SecretOwner>,
	pub now: DateTime<Utc>,

	/// Fleet project directory, containing fleet.nix file.
	pub directory: PathBuf,
	/// builtins.currentSystem
	pub local_system: String,
	pub data: Arc<FleetData>,
	/// fleet_config.config
	pub config_field: Value,
	/// flake.output
	pub flake_outputs: Value,
	// TODO: Remove with connectivity refactor
	pub localhost: String,

	/// import nixpkgs {system = local};
	pub default_pkgs: Value,
	/// inputs.nixpkgs
	pub nixpkgs: Value,

	pub local_host: OnceLock<Arc<ConfigHost>>,
}

// TODO: Make field not pub
#[derive(Clone)]
pub struct Config(pub Arc<FleetConfigInternals>);

impl Deref for Config {
	type Target = FleetConfigInternals;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

#[derive(Clone, PartialEq, Copy, Debug)]
pub enum DeployKind {
	/// NixOS => NixOS managed by fleet
	UpgradeToFleet,
	/// NixOS managed by fleet => NixOS managed by fleet
	Fleet,
	/// Remote host has /mnt, /mnt/boot mounted,
	/// generated config is added to fleet configuration.
	NixosInstall,
	/// Remote host has some system and nix installed in multi-user mode (/nix is owned by root),
	/// generated config is added to fleet configuration,
	/// and /etc/NIXOS_LUSTRATE exists, fleet will perform the rest.
	NixosLustrate,
}

impl FromStr for DeployKind {
	type Err = anyhow::Error;
	fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
		match s {
			"upgrade-to-fleet" => Ok(Self::UpgradeToFleet),
			"fleet" => Ok(Self::Fleet),
			"nixos-install" => Ok(Self::NixosInstall),
			"nixos-lustrate" => Ok(Self::NixosLustrate),
			v => bail!(
				"unknown deploy_kind: {v}; expected on of \"upgrade-to-fleet\", \"fleet\", \"nixos-install\", \"nixos-lustrate\""
			),
		}
	}
}
pub struct ConfigHost {
	config: Config,
	pub name: String,
	groups: OnceLock<Vec<String>>,

	// TODO: Both of those values are taken from host opts, there should be a cleaner way to specify it
	deploy_kind: OnceLock<DeployKind>,
	session_destination: OnceLock<String>,
	legacy_ssh_store: OnceLock<bool>,

	pub host_config: Option<Value>,
	pub nixos_config: OnceLock<Value>,
	pub nixos_unchecked_config: OnceLock<Value>,
	pub pkgs_override: Option<Value>,

	// TODO: Move command helpers away with connectivity refactor
	pub local: bool,
	pub remowt: OnceLock<Remowt>,
	nix_store: OnceLock<Arc<Store>>,
	nix_plugin: tokio::sync::OnceCell<()>,
}

const NIX_PLUGIN_ID: u16 = 2;

fn agents_dir() -> Result<PathBuf> {
	std::env::var_os("REMOWT_AGENTS_DIR")
		.map(PathBuf::from)
		.or_else(|| option_env!("REMOWT_AGENTS_DIR").map(PathBuf::from))
		.ok_or_else(|| {
			anyhow!("no remowt-agents bundle; set REMOWT_AGENTS_DIR to a remowt-agents output")
		})
}

fn agent_bundle() -> Result<AgentBundle> {
	AgentBundle::from_dir(agents_dir()?)
}

#[derive(Debug, Clone, Copy)]
pub enum GenerationStorage {
	Deployer,
	Machine,
	Pusher,
}
impl GenerationStorage {
	fn prefix(&self) -> &'static str {
		match self {
			GenerationStorage::Deployer => "deployer.",
			GenerationStorage::Machine => "",
			GenerationStorage::Pusher => "pusher.",
		}
	}
}

#[derive(Tabled, Debug)]
pub struct Generation {
	#[tabled(rename = "ID", format("{}", self.rollback_id()))]
	pub id: u32,
	#[tabled(rename = "Current")]
	pub current: bool,
	#[tabled(rename = "Created at")]
	pub datetime: UtcDateTime,
	#[tabled(format = "{:?}")]
	pub store_path: Utf8PathBuf,
	#[tabled(skip)]
	pub location: GenerationStorage,
}
impl Generation {
	pub fn rollback_id(&self) -> String {
		format!("{}{}", self.location.prefix(), self.id)
	}
}

impl ConfigHost {
	pub async fn list_generations(&self, profile: &str) -> Result<Vec<Generation>> {
		let plugin_id = self.ensure_nix_plugin().await?;
		let nix = self
			.remowt()
			.await?
			.plugin_endpoints::<remowt_fleet::NixClient<_>>(plugin_id);
		let raw = nix
			.list_generations(profile.to_owned())
			.await
			.map_err(|e| anyhow!("{e:?}"))?
			.map_err(|e| anyhow!("{e}"))?;
		raw.into_iter()
			.map(|g| {
				let id: u32 =
					g.id.try_into()
						.with_context(|| format!("generation id {} doesn't fit in u32", g.id))?;
				let datetime = UtcDateTime::from_unix_timestamp(g.creation_time_unix)
					.with_context(|| {
						format!("invalid generation timestamp {}", g.creation_time_unix)
					})?;
				Ok(Generation {
					id,
					current: g.current,
					datetime,
					store_path: g.store_path,
					location: GenerationStorage::Machine,
				})
			})
			.collect()
	}

	pub fn set_session_destination(&self, dest: String) {
		self.session_destination
			.set(dest)
			.expect("session destination is already set")
	}
	pub fn set_deploy_kind(&self, kind: DeployKind) {
		self.deploy_kind
			.set(kind)
			.expect("deploy kind is already set");
	}
	pub fn set_legacy_ssh_store(&self, legacy: bool) {
		self.legacy_ssh_store
			.set(legacy)
			.expect("legacy ssh store is already set")
	}
	pub async fn deploy_kind(&self) -> Result<DeployKind> {
		if let Some(kind) = self.deploy_kind.get() {
			return Ok(*kind);
		}
		let remowt = self.remowt().await?;
		let fs = remowt.endpoints::<FsClient<_>>();
		let is_fleet_managed = match fs.file_exists(Utf8PathBuf::from("/etc/FLEET_HOST")).await {
			Ok(v) => v,
			Err(e) => {
				bail!("failed to query remote system kind: {e}");
			}
		};
		if !is_fleet_managed {
			bail!(
				"{}",
				indoc::indoc! {"
				host is not marked as managed by fleet
				if you're not trying to lustrate/install system from scratch,
				you should either
					1. manually create /etc/FLEET_HOST file on the target host,
					2. use ?deploy_kind=fleet host argument if you're upgrading from older version of fleet
					3. use ?deploy_kind=upgrade_to_fleet if you're upgrading from plain nixos to fleet-managed nixos
				for installation use ?deploy_kind=nixos_install / ?deploy_kind=nixos_lustrate 
			"}
			);
		}
		// TOCTOU is possible
		let _ = self.deploy_kind.set(DeployKind::Fleet);
		Ok(*self.deploy_kind.get().expect("deploy kind is just set"))
	}
	async fn connection(&self) -> Result<Remowt> {
		if let Some(conn) = self.remowt.get() {
			return Ok(conn.clone());
		}
		let bundle = agent_bundle()?;
		let conn = if self.local {
			Remowt::connect_local(&bundle, "remowt-fleet".to_owned())
				.await
				.context("starting local remowt agent")?
		} else {
			let dest = self
				.session_destination
				.get()
				.cloned()
				.unwrap_or_else(|| self.name.clone());
			Remowt::connect(&dest, &bundle, "remowt-fleet".to_owned())
				.await
				.map_err(|e| anyhow!("remowt error while connecting to {}: {e:#?}", self.name))?
		};
		PromptEndpoints(PrependSourcePrompter {
			prompter: AutoPrompter::new().await,
			source: if self.local {
				vec![]
			} else {
				vec![Source(std::borrow::Cow::Owned(format!(
					"ssh host: {}",
					self.name
				)))]
			},
			description: "".to_owned(),
		})
		.register_endpoints(&mut conn.rpc());
		let _ = self.remowt.set(conn);
		Ok(self.remowt.get().expect("just set").clone())
	}

	/// Client for this host's unprivileged agent.
	pub async fn remowt(&self) -> Result<Remowt> {
		self.connection().await
	}

	pub fn ensure_nix_plugin(&self) -> Pin<Box<dyn Future<Output = Result<u16>> + Send + '_>> {
		Box::pin(async {
			self.nix_plugin
				.get_or_try_init(|| async {
					let pkgs = self.pkgs()?;
					let name = "remowt-plugin-fleet";
					let plugin = nix_go!(pkgs[{ name }]);
					let built = plugin
						.build("out")
						.context("failed to build the fleet nix plugin")?;
					let copied = self
						.remote_derivation(&built)
						.await
						.context("failed to copy the fleet nix plugin to the host store")?;
					let bin = copied.join("bin/remowt-plugin-fleet");
					self.remowt()
						.await?
						.run0_load_plugin_path(NIX_PLUGIN_ID, bin.as_str())
						.await
						.context("failed to load the fleet nix plugin")?;
					self.remowt()
						.await?
						.rpc()
						.wait_for_connection_to(Address::Plugin(NIX_PLUGIN_ID))
						.await
						.map_err(|_| anyhow!("failed to wait for plugin"))?;
					anyhow::Ok(())
				})
				.await?;
			Ok(NIX_PLUGIN_ID)
		})
	}

	async fn nix_store(&self) -> Result<Arc<Store>> {
		if let Some(store) = self.nix_store.get() {
			return Ok(store.clone());
		}
		let conn = self.connection().await?;
		let socket = match self.deploy_kind().await? {
			DeployKind::NixosInstall => {
				remowt_fleet::nix_store_socket(conn, "/mnt?require-sigs=false").await?
			}
			_ => remowt_fleet::nix_store_socket(conn, "auto").await?,
		};
		let uri = format!("unix://{}", socket.display());
		let store = Arc::new(Store::open(&uri)?);
		let _ = self.nix_store.set(store);
		Ok(self.nix_store.get().expect("just set").clone())
	}

	pub async fn decrypt(&self, data: SecretData) -> Result<Vec<u8>> {
		ensure!(data.encrypted, "secret is not encrypted");
		let remowt = self.remowt().await?;
		let mut cmd = remowt.cmd("fleet-install-secrets");
		cmd.arg("decrypt").eqarg("--secret", data.to_string());
		let encoded = cmd
			.sudo()
			.run_string()
			.await
			.context("failed to call remote host for decrypt")?;
		let data: SecretData = encoded.parse().map_err(|e| anyhow!("{e}"))?;
		ensure!(!data.encrypted, "secret came out encrypted");
		Ok(data.data)
	}
	pub async fn reencrypt_distribution(
		&self,
		data: &FleetSecretDistribution,
		targets: BTreeSet<SecretOwner>,
		now: DateTime<Utc>,
	) -> Result<FleetSecretDistribution> {
		let mut parts = BTreeMap::new();
		for (part_name, part) in &data.secret.parts {
			parts.insert(
				part_name.clone(),
				if part.raw.encrypted {
					FleetSecretPart {
						raw: self.reencrypt(part.raw.clone(), targets.clone()).await?,
					}
				} else {
					part.clone()
				},
			);
		}
		let secret = FleetSecretData {
			created_at: data.secret.created_at,
			expires_at: data.secret.expires_at,
			generation_data: data.secret.generation_data.clone(),
			parts,
		};
		Ok(FleetSecretDistribution::new(targets, secret, now))
	}
	pub async fn reencrypt(
		&self,
		data: SecretData,
		targets: BTreeSet<SecretOwner>,
	) -> Result<SecretData> {
		let remowt = self.remowt().await?;
		ensure!(data.encrypted, "secret is not encrypted");
		let mut cmd = remowt.cmd("fleet-install-secrets");
		cmd.arg("reencrypt").eqarg("--secret", data.to_string());
		for target in targets {
			let key = self.config.key(&target).await?;
			cmd.eqarg("--targets", key);
		}
		let encoded = cmd
			.sudo()
			.run_string()
			.await
			.context("failed to call remote host for decrypt")?;
		let data: SecretData = encoded.parse().map_err(|e| anyhow!("{e}"))?;
		ensure!(data.encrypted, "secret came out not encrypted");
		Ok(data)
	}
	/// Returns path for futureproofing, as path might change i.e on conversion to CA
	pub async fn remote_derivation(&self, path: impl AsRef<Utf8Path>) -> Result<Utf8PathBuf> {
		let path = path.as_ref().to_owned();
		if self.local {
			// Path is located locally, thus already trusted.
			return Ok(path);
		}
		let sign: Pin<Box<dyn Future<Output = Result<()>> + Send>> = {
			let path = path.clone();
			Box::pin(async move {
				let local = self.config.local_host();
				let plugin_id = local.ensure_nix_plugin().await?;
				let nix = local
					.remowt()
					.await?
					.plugin_endpoints::<remowt_fleet::NixClient<_>>(plugin_id);
				nix.sign_closure(path, Utf8PathBuf::from("/etc/nix/private-key"))
					.await
					.map_err(|e| anyhow!("{e:?}"))?
					.map_err(|e| anyhow!("{e}"))?;
				Ok(())
			})
		};
		if let Err(e) = sign.await {
			warn!("failed to sign store paths: {e}");
		}
		let store = self.nix_store().await?;
		{
			let path = path.clone();
			let eval_store = eval_store();
			spawn_blocking(move || eval_store.copy_to(&store, path.as_ref()))
				.await
				.expect("copy_to panicked")
				.context("copying closure to remote store")?;
		}
		Ok(path)
	}
}

struct HostSecretDefinition(Value);

impl ConfigHost {
	// TOCTOU is possible here in case if config is changed, but this case is not handled anywhere anyway,
	// assuming getting tags always returns the same value.
	pub fn tags(&self) -> Result<Vec<String>> {
		if let Some(v) = self.groups.get() {
			return Ok(v.clone());
		}
		let Some(host_config) = &self.host_config else {
			return Ok(vec![]);
		};
		let tags: Vec<String> = nix_go_json!(host_config.tags);

		let _ = self.groups.set(tags.clone());

		Ok(tags)
	}
	pub fn nixos_config(&self) -> Result<Value> {
		if let Some(v) = self.nixos_config.get() {
			return Ok(v.clone());
		}
		let Some(host_config) = &self.host_config else {
			bail!("local host has no nixos_config");
		};
		let nixos_config = nix_go!(host_config.nixos.config);
		assert_warn("nixos config evaluation", &nixos_config)?;

		let _ = self.nixos_config.set(nixos_config.clone());

		Ok(nixos_config)
	}
	pub fn nixos_unchecked_config(&self) -> Result<Value> {
		if let Some(v) = self.nixos_unchecked_config.get() {
			return Ok(v.clone());
		}
		let Some(host_config) = &self.host_config else {
			bail!("local host has no nixos_config");
		};
		let nixos_config = nix_go!(host_config.nixos_unchecked.config);

		let _ = self.nixos_unchecked_config.set(nixos_config.clone());

		Ok(nixos_config)
	}

	pub fn list_defined_secrets(&self) -> Result<Vec<String>> {
		let nixos = self.nixos_unchecked_config()?;
		let secrets = nix_go!(nixos.secrets);
		secrets.list_fields()
	}

	/// Packages for this host, resolved with nixpkgs overlays
	pub fn pkgs(&self) -> Result<Value> {
		if let Some(value) = &self.pkgs_override {
			return Ok(value.clone());
		}
		let Some(host_config) = &self.host_config else {
			bail!("local host has no host_config");
		};
		// TODO: Should nixos.options be cached?
		Ok(nix_go!(host_config.nixos.options._module.args.value.pkgs))
	}
}

#[derive(Clone)]
pub struct SharedSecretDefinition(Value);
impl SharedSecretDefinition {
	pub fn expected_owners(&self) -> Result<BTreeSet<SecretOwner>> {
		let secret = &self.0;
		Ok(nix_go_json!(secret.expectedOwners))
	}
	pub fn allow_different(&self) -> Result<bool> {
		let secret = &self.0;
		Ok(nix_go_json!(secret.allowDifferent))
	}
	pub fn regenerate_on_owner_added(&self) -> Result<bool> {
		let secret = &self.0;
		Ok(nix_go_json!(secret.regenerateOnOwnerAdded))
	}
	pub fn regenerate_on_owner_removed(&self) -> Result<bool> {
		let secret = &self.0;
		Ok(nix_go_json!(secret.regenerateOnOwnerRemoved))
	}
	pub fn generator(&self) -> Result<Value> {
		let secret = &self.0;
		Ok(nix_go!(secret.generator))
	}
}

impl Config {
	pub fn tagged_hostnames(&self, tag: &str) -> Result<Vec<String>> {
		let config = &self.config_field;
		let tagged: Vec<String> = nix_go_json!(config.taggedWith[{ tag }]);
		Ok(tagged)
	}
	pub fn expand_owner_set(&self, owners: Vec<String>) -> Result<BTreeSet<String>> {
		let mut out = BTreeSet::new();
		for owner in owners {
			if let Some(tag) = owner.strip_prefix('@') {
				let hosts = self.tagged_hostnames(tag)?;
				out.extend(hosts);
			} else {
				out.insert(owner);
			}
		}
		Ok(out)
	}
	pub fn local_host(&self) -> Arc<ConfigHost> {
		self.local_host
			.get_or_init(|| {
				Arc::new(ConfigHost {
					config: self.clone(),
					name: "<virtual localhost>".to_owned(),
					host_config: None,
					nixos_config: OnceLock::new(),
					nixos_unchecked_config: OnceLock::new(),
					groups: {
						let cell = OnceLock::new();
						let _ = cell.set(vec![]);
						cell
					},
					pkgs_override: Some(self.default_pkgs.clone()),

					local: true,
					remowt: OnceLock::new(),
					nix_store: OnceLock::new(),
					nix_plugin: tokio::sync::OnceCell::new(),
					deploy_kind: OnceLock::new(),
					session_destination: OnceLock::new(),
					legacy_ssh_store: OnceLock::new(),
				})
			})
			.clone()
	}

	pub fn preferred_hosts(
		&self,
		filter: impl Fn(&str) -> bool,
	) -> Result<impl Iterator<Item = Result<ConfigHost>>> {
		let prefer = self
			.prefer_identities
			.iter()
			.filter_map(|v| v.as_host())
			.collect::<HashSet<_>>();
		let config = &self.config_field;
		let mut names = nix_go!(config.hosts).list_fields()?;
		names.retain(|s| filter(s));
		names.sort_by_key(|h| prefer.contains(h.as_str()));

		Ok(names.into_iter().map(|h| self.host(&h)))
	}

	pub fn host(&self, name: &str) -> Result<ConfigHost> {
		let config = &self.config_field;
		let host_config = nix_go!(config.hosts[{ name }]);

		Ok(ConfigHost {
			config: self.clone(),
			name: name.to_owned(),
			host_config: Some(host_config),
			nixos_config: OnceLock::new(),
			nixos_unchecked_config: OnceLock::new(),
			groups: OnceLock::new(),
			pkgs_override: None,

			// TODO: Remove with connectivit refactor
			local: self.localhost == name,
			remowt: OnceLock::new(),
			nix_store: OnceLock::new(),
			nix_plugin: tokio::sync::OnceCell::new(),
			deploy_kind: OnceLock::new(),
			session_destination: OnceLock::new(),
			legacy_ssh_store: OnceLock::new(),
		})
	}
	pub fn list_hosts(&self) -> Result<Vec<ConfigHost>> {
		let config = &self.config_field;
		let names = nix_go!(config.hosts).list_fields()?;
		let mut out = vec![];
		for name in names {
			out.push(self.host(&name)?);
		}
		Ok(out)
	}
	// TODO: Replace usages with .host().nixos_config
	pub fn system_config(&self, host: &str) -> Result<Value> {
		let fleet_field = &self.config_field;
		Ok(nix_go!(fleet_field.hosts[{ host }].nixos.config))
	}

	pub fn secret_definition(&self, secret: &str) -> Result<Option<SharedSecretDefinition>> {
		let config = &self.config_field;
		let shared_secrets = nix_go!(config.secrets);
		if !shared_secrets.has_field(secret)? {
			return Ok(None);
		}
		Ok(Some(SharedSecretDefinition(nix_go!(
			shared_secrets[secret]
		))))
	}

	pub fn save(&self) -> Result<()> {
		let mut tempfile = NamedTempFile::new_in(self.directory.clone()).context("failed to create updated version of fleet.nix in the same directory as original.\nDo you have write access to it? Access only to the fleet.nix won't be enough, the directory is used for atomic overwrite operation.\nIt is not recommended to use fleet by root anyway, move fleet project to your home directory.")?;
		let data = nixlike::serialize(&*self.data)?;
		tempfile.write_all(
			format!(
				"# This file contains fleet state and shouldn't be edited by hand\n\n{data}\n\n# vim: ts=2 et nowrap\n"
			)
			.as_bytes(),
		)?;
		let mut fleet_data_path = self.directory.clone();
		fleet_data_path.push("fleet.nix");
		tempfile.persist(fleet_data_path)?;
		Ok(())
	}
}
