use std::{
	cell::OnceCell,
	collections::BTreeSet,
	ffi::{OsStr, OsString},
	fmt::Display,
	io::Write,
	ops::Deref,
	path::PathBuf,
	str::FromStr,
	sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use fleet_shared::SecretData;
use nix_eval::{NixSession, Value, nix_go, nix_go_json, util::assert_warn};
use openssh::SessionBuilder;
use serde::de::DeserializeOwned;
use tabled::Tabled;
use tempfile::NamedTempFile;
use time::{UtcDateTime, format_description};
use tracing::warn;

use crate::{
	command::MyCommand,
	fleetdata::{FleetData, FleetSecret, FleetSharedSecret},
};

pub struct FleetConfigInternals {
	/// Fleet project directory, containing fleet.nix file.
	pub directory: PathBuf,
	/// builtins.currentSystem
	pub local_system: String,
	pub data: Mutex<FleetData>,
	pub nix_args: Vec<OsString>,
	/// fleet_config.config
	pub config_field: Value,
	// TODO: Remove with connectivity refactor
	pub localhost: String,

	/// import nixpkgs {system = local};
	pub default_pkgs: Value,
	/// inputs.nixpkgs
	pub nixpkgs: Value,

	pub nix_session: NixSession,
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

#[derive(Clone, Copy, Debug)]
pub enum EscalationStrategy {
	Sudo,
	Run0,
	Su,
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
	groups: OnceCell<Vec<String>>,

	deploy_kind: OnceCell<DeployKind>,

	pub host_config: Option<Value>,
	pub nixos_config: OnceCell<Value>,
	pub nixos_unchecked_config: OnceCell<Value>,
	pub pkgs_override: Option<Value>,

	// TODO: Move command helpers away with connectivity refactor
	pub local: bool,
	pub session: OnceLock<Arc<openssh::Session>>,
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
	pub store_path: PathBuf,
	#[tabled(skip)]
	pub location: GenerationStorage,
}
impl Generation {
	pub fn rollback_id(&self) -> String {
		format!("{}{}", self.location.prefix(), self.id)
	}
}

fn parse_generation_line(g: &str) -> Option<Generation> {
	let mut parts = g.split_whitespace();
	let id = parts.next()?;
	let id: u32 = id.parse().ok()?;
	let date = parts.next()?;
	let time = parts.next()?;
	let current = if let Some(current) = parts.next() {
		if current == "(current)" {
			Some(true)
		} else {
			None
		}
	} else {
		Some(false)
	};
	let current = current?;
	if parts.next().is_some() {
		warn!("unexpected text after generation: {g}");
	}

	let format = format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
		.expect("valid format");
	let datetime = UtcDateTime::parse(&format!("{date} {time}"), &format).ok()?;

	Some(Generation {
		id,
		current,
		datetime,
		store_path: PathBuf::new(),
		location: GenerationStorage::Machine,
	})
}
// TODO: Move command helpers away with connectivity refactor
impl ConfigHost {
	pub async fn list_generations(&self, profile: &str) -> Result<Vec<Generation>> {
		let mut cmd = self.cmd("nix-env").await?;
		cmd.comparg("--profile", format!("/nix/var/nix/profiles/{profile}"))
			.arg("--list-generations")
			.env("TZ", "UTC");
		// Sudo is required because --list-generations tries to acquire profile lock
		let data = cmd.sudo().run_string().await?;
		let mut generations = data
			.split('\n')
			.map(|e| e.trim())
			.filter(|&l| !l.is_empty())
			.filter_map(|g| {
				let generation = parse_generation_line(g);
				if generation.is_none() {
					warn!("bad generation: {g}");
				};
				generation
			})
			.collect::<Vec<_>>();
		for ele in generations.iter_mut() {
			let mut cmd = self.cmd("readlink").await?;
			cmd.arg("--")
				.arg(format!("/nix/var/nix/profiles/{profile}-{}-link", ele.id));
			let path = cmd.run_string().await?;
			ele.store_path = PathBuf::from(path.trim_end_matches("\n"));
		}

		Ok(generations)
	}

	pub fn set_deploy_kind(&self, kind: DeployKind) {
		self.deploy_kind
			.set(kind)
			.ok()
			.expect("deploy kind is already set");
	}
	pub async fn deploy_kind(&self) -> Result<DeployKind> {
		if let Some(kind) = self.deploy_kind.get() {
			return Ok(kind.clone());
		}
		let is_fleet_managed = match self.file_exists("/etc/FLEET_HOST").await {
			Ok(v) => v,
			Err(e) => {
				bail!("failed to query remote system kind: {}", e);
			}
		};
		if !is_fleet_managed {
			bail!(indoc::indoc! {"
				host is not marked as managed by fleet
				if you're not trying to lustrate/install system from scratch,
				you should either
					1. manually create /etc/FLEET_HOST file on the target host,
					2. use ?deploy_kind=fleet host argument if you're upgrading from older version of fleet
					3. use ?deploy_kind=upgrade_to_fleet if you're upgrading from plain nixos to fleet-managed nixos
			"});
		}
		// TOCTOU is possible
		let _ = self.deploy_kind.set(DeployKind::Fleet);
		Ok(self
			.deploy_kind
			.get()
			.expect("deploy kind is just set")
			.clone())
	}
	pub async fn escalation_strategy(&self) -> Result<EscalationStrategy> {
		// Prefer sudo, as run0 has some gotchas with polkit
		// and too many repeating prompts.
		if (self.find_in_path("sudo").await).is_ok() {
			return Ok(EscalationStrategy::Sudo);
		}
		if (self.find_in_path("run0").await).is_ok() {
			return Ok(EscalationStrategy::Run0);
		}
		Ok(EscalationStrategy::Su)
	}
	async fn open_session(&self) -> Result<Arc<openssh::Session>> {
		assert!(!self.local, "do not open ssh connection to local session");
		// FIXME: TOCTOU
		if let Some(session) = &self.session.get() {
			return Ok((*session).clone());
		};
		let session = SessionBuilder::default();
		let session = session
			.connect(&self.name)
			.await
			.map_err(|e| anyhow!("ssh error while connecting to {}: {e:#?}", self.name))?;
		let session = Arc::new(session);
		self.session.set(session.clone()).expect("TOCTOU happened");
		Ok(session)
	}
	pub async fn mktemp_dir(&self) -> Result<String> {
		let mut cmd = self.cmd("mktemp").await?;
		cmd.arg("-d");
		let path = cmd.run_string().await?;
		Ok(path.trim_end().to_owned())
	}
	pub async fn file_exists(&self, path: impl AsRef<OsStr>) -> Result<bool> {
		let mut cmd = self.cmd("sh").await?;
		cmd.arg("-c")
			.arg("test -e \"$1\" && echo true || echo false")
			.arg("_")
			.arg(path);
		Ok(cmd.run_value().await?)
	}
	pub async fn read_file_bin(&self, path: impl AsRef<OsStr>) -> Result<Vec<u8>> {
		let mut cmd = self.cmd("cat").await?;
		cmd.arg(path);
		cmd.run_bytes().await
	}
	pub async fn read_file_text(&self, path: impl AsRef<OsStr>) -> Result<String> {
		let mut cmd = self.cmd("cat").await?;
		cmd.arg(path);
		cmd.run_string().await
	}
	pub async fn read_dir(&self, path: impl AsRef<OsStr>) -> Result<Vec<String>> {
		let mut cmd = self.cmd("ls").await?;
		cmd.arg(path);
		let out = cmd.run_string().await?;
		let mut lines = out.split('\n');
		if let Some(last) = lines.next_back() {
			ensure!(last.is_empty(), "output of ls should end with newline");
		}
		Ok(lines.map(ToOwned::to_owned).collect())
	}
	#[allow(dead_code)]
	pub async fn read_file_json<D: DeserializeOwned>(&self, path: impl AsRef<OsStr>) -> Result<D> {
		let text = self.read_file_text(path).await?;
		Ok(serde_json::from_str(&text)?)
	}
	pub async fn read_env(&self, env: &str) -> Result<String> {
		let mut cmd = self.cmd("printenv").await?;
		cmd.arg(env);
		cmd.run_string().await
	}
	pub async fn find_in_path(&self, command: &str) -> Result<String> {
		// // `which` is not a part of coreutils, and it might not exist on machine.
		// let path = self.read_env("PATH").await?;
		// // Assuming delimiter is :, we don't work with windows host, this check will be much
		// // more sophisticated in remowt backend (and quicker, since actual PATH search will be done on remote machine)
		// for ele in path.split(':') {
		// 	let test_path = format!("{ele}/{cmd}");
		// 	test -x etc
		// }
		// let mut cmd = self.cmd("printenv").await?;
		// cmd.arg(env);
		// Ok(cmd.run_string().await?)
		// Assuming this is an environment issue if which doesn't exist, will be fixed with remowt.
		let mut cmd = self
			.cmd_escalation(
				// Not used
				EscalationStrategy::Su,
				"which",
			)
			.await?;
		cmd.arg(command);
		cmd.run_string().await
	}
	pub async fn read_file_value<D: FromStr>(&self, path: impl AsRef<OsStr>) -> Result<D>
	where
		<D as FromStr>::Err: Display,
	{
		let text = self.read_file_text(path).await?;
		D::from_str(&text).map_err(|e| anyhow!("failed to parse value: {e}"))
	}
	pub async fn cmd(&self, cmd: impl AsRef<OsStr>) -> Result<MyCommand> {
		self.cmd_escalation(self.escalation_strategy().await?, cmd)
			.await
	}
	pub async fn cmd_escalation(
		&self,
		escalation: EscalationStrategy,
		cmd: impl AsRef<OsStr>,
	) -> Result<MyCommand> {
		if self.local {
			Ok(MyCommand::new(escalation, cmd))
		} else {
			let session = self.open_session().await?;
			Ok(MyCommand::new_on(escalation, cmd, session))
		}
	}
	pub async fn nix_cmd(&self) -> Result<MyCommand> {
		let mut nix = self.cmd("nix").await?;
		nix.args([
			"--extra-experimental-features",
			"nix-command",
			"--extra-experimental-features",
			"flakes",
		]);
		Ok(nix)
	}

	pub async fn decrypt(&self, data: SecretData) -> Result<Vec<u8>> {
		ensure!(data.encrypted, "secret is not encrypted");
		let mut cmd = self.cmd("fleet-install-secrets").await?;
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
	pub async fn reencrypt(&self, data: SecretData, targets: Vec<String>) -> Result<SecretData> {
		ensure!(data.encrypted, "secret is not encrypted");
		let mut cmd = self.cmd("fleet-install-secrets").await?;
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
	pub async fn remote_derivation(&self, path: &PathBuf) -> Result<PathBuf> {
		if self.local {
			// Path is located locally, thus already trusted.
			return Ok(path.to_owned());
		}
		let mut nix = MyCommand::new(
			// Not used
			EscalationStrategy::Su,
			"nix",
		);
		nix.arg("copy").arg("--substitute-on-destination");

		match self.deploy_kind().await? {
			DeployKind::Fleet | DeployKind::UpgradeToFleet | DeployKind::NixosLustrate => {
				nix.comparg("--to", format!("ssh-ng://{}", self.name));
			}
			DeployKind::NixosInstall => {
				nix
					// Signature checking makes no sense with remote-store store argument set, as we're not even interacting with remote nix daemon
					.arg("--no-check-sigs")
					.comparg(
						"--to",
						format!("ssh-ng://root@{}?remote-store=/mnt", self.name),
					);
			}
		}
		nix.arg(path);
		nix.run_nix().await.context("nix copy")?;
		Ok(path.to_owned())
	}
	pub async fn systemctl_stop(&self, name: &str) -> Result<()> {
		let mut cmd = self.cmd("systemctl").await?;
		cmd.arg("stop").arg(name);
		cmd.sudo().run().await
	}
	pub async fn systemctl_start(&self, name: &str) -> Result<()> {
		let mut cmd = self.cmd("systemctl").await?;
		cmd.arg("start").arg(name);
		cmd.sudo().run().await
	}

	pub async fn rm_file(&self, path: impl AsRef<OsStr>, sudo: bool) -> Result<()> {
		let mut cmd = self.cmd("rm").await?;
		cmd.arg("-f").arg(path);
		if sudo {
			cmd = cmd.sudo()
		}
		cmd.run().await
	}
}
impl ConfigHost {
	// TOCTOU is possible here in case if config is changed, but this case is not handled anywhere anyway,
	// assuming getting tags always returns the same value.
	pub async fn tags(&self) -> Result<Vec<String>> {
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
	pub async fn nixos_config(&self) -> Result<Value> {
		if let Some(v) = self.nixos_config.get() {
			return Ok(v.clone());
		}
		let Some(host_config) = &self.host_config else {
			bail!("local host has no nixos_config");
		};
		let nixos_config = nix_go!(host_config.nixos.config);
		assert_warn("nixos config evaluation", &nixos_config).await?;

		let _ = self.nixos_config.set(nixos_config.clone());

		Ok(nixos_config)
	}
	pub async fn nixos_unchecked_config(&self) -> Result<Value> {
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

	pub async fn list_configured_secrets(&self) -> Result<Vec<String>> {
		let nixos = self.nixos_unchecked_config().await?;
		let secrets = nix_go!(nixos.secrets);
		let mut out = Vec::new();
		for name in secrets.list_fields().await? {
			let secret = nix_go!(secrets[{ name }]);
			let is_shared: bool = nix_go_json!(secret.shared);
			if is_shared {
				continue;
			}
			out.push(name);
		}
		Ok(out)
	}
	pub async fn secret_field(&self, name: &str) -> Result<Value> {
		let nixos = self.nixos_unchecked_config().await?;
		Ok(nix_go!(nixos.secrets[{ name }]))
	}

	/// Packages for this host, resolved with nixpkgs overlays
	pub async fn pkgs(&self) -> Result<Value> {
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

impl Config {
	pub async fn tagged_hostnames(&self, tag: &str) -> Result<Vec<String>> {
		let config = &self.config_field;
		let tagged: Vec<String> = nix_go_json!(config.taggedWith[{ tag }]);
		Ok(tagged)
	}
	pub async fn expand_owner_set(&self, owners: Vec<String>) -> Result<BTreeSet<String>> {
		let mut out = BTreeSet::new();
		for owner in owners {
			if let Some(tag) = owner.strip_prefix('@') {
				let hosts = self.tagged_hostnames(tag).await?;
				out.extend(hosts);
			} else {
				out.insert(owner);
			}
		}
		Ok(out)
	}
	pub fn local_host(&self) -> ConfigHost {
		ConfigHost {
			config: self.clone(),
			name: "<virtual localhost>".to_owned(),
			host_config: None,
			nixos_config: OnceCell::new(),
			nixos_unchecked_config: OnceCell::new(),
			groups: {
				let cell = OnceCell::new();
				let _ = cell.set(vec![]);
				cell
			},
			pkgs_override: Some(self.default_pkgs.clone()),

			local: true,
			session: OnceLock::new(),
			deploy_kind: OnceCell::new(),
		}
	}

	pub async fn host(&self, name: &str) -> Result<ConfigHost> {
		let config = &self.config_field;
		let host_config = nix_go!(config.hosts[{ name }]);

		Ok(ConfigHost {
			config: self.clone(),
			name: name.to_owned(),
			host_config: Some(host_config),
			nixos_config: OnceCell::new(),
			nixos_unchecked_config: OnceCell::new(),
			groups: OnceCell::new(),
			pkgs_override: None,

			// TODO: Remove with connectivit refactor
			local: self.localhost == name,
			session: OnceLock::new(),
			deploy_kind: OnceCell::new(),
		})
	}
	pub async fn list_hosts(&self) -> Result<Vec<ConfigHost>> {
		let config = &self.config_field;
		let names = nix_go!(config.hosts).list_fields().await?;
		let mut out = vec![];
		for name in names {
			out.push(self.host(&name).await?);
		}
		Ok(out)
	}
	// TODO: Replace usages with .host().nixos_config
	pub async fn system_config(&self, host: &str) -> Result<Value> {
		let fleet_field = &self.config_field;
		Ok(nix_go!(fleet_field.hosts[{ host }].nixos.config))
	}

	/// Shared secrets configured in fleet.nix or in flake
	pub async fn list_configured_shared(&self) -> Result<Vec<String>> {
		let config_field = &self.config_field;
		Ok(nix_go!(config_field.sharedSecrets).list_fields().await?)
	}
	/// Shared secrets configured in fleet.nix
	pub fn list_shared(&self) -> Vec<String> {
		let data = self.data();
		data.shared_secrets.keys().cloned().collect()
	}
	pub fn has_shared(&self, name: &str) -> bool {
		let data = self.data();
		data.shared_secrets.contains_key(name)
	}
	pub fn replace_shared(&self, name: String, shared: FleetSharedSecret) {
		let mut data = self.data_mut();
		data.shared_secrets.insert(name.to_owned(), shared);
	}
	pub fn remove_shared(&self, secret: &str) {
		let mut data = self.data_mut();
		data.shared_secrets.remove(secret);
	}

	pub fn list_secrets(&self, host: &str) -> Vec<String> {
		let data = self.data();
		let Some(secrets) = data.host_secrets.get(host) else {
			return Vec::new();
		};
		secrets.keys().cloned().collect()
	}

	pub fn has_secret(&self, host: &str, secret: &str) -> bool {
		let data = self.data();
		let Some(host_secrets) = data.host_secrets.get(host) else {
			return false;
		};
		host_secrets.contains_key(secret)
	}
	pub fn insert_secret(&self, host: &str, secret: String, value: FleetSecret) {
		let mut data = self.data_mut();
		let host_secrets = data.host_secrets.entry(host.to_owned()).or_default();
		host_secrets.insert(secret, value);
	}

	pub fn host_secret(&self, host: &str, secret: &str) -> Result<FleetSecret> {
		let data = self.data();
		let Some(host_secrets) = data.host_secrets.get(host) else {
			bail!("no secrets for machine {host}");
		};
		let Some(secret) = host_secrets.get(secret) else {
			bail!("machine {host} has no secret {secret}");
		};
		Ok(secret.clone())
	}
	pub fn shared_secret(&self, secret: &str) -> Result<FleetSharedSecret> {
		let data = self.data();
		let Some(secret) = data.shared_secrets.get(secret) else {
			bail!("no shared secret {secret}");
		};
		Ok(secret.clone())
	}
	pub async fn shared_secret_expected_owners(&self, secret: &str) -> Result<Vec<String>> {
		let config_field = &self.config_field;
		Ok(nix_go_json!(
			config_field.sharedSecrets[{ secret }].expectedOwners
		))
	}

	// TODO: Should this be something modifiable from other processes?
	// E.g terraform provider might want to update FleetData (e.g secrets),
	// and current implementation assumes only one process holds current fleet.nix
	// Given that it is no longer needs to be a file for nix evaluation,
	// maybe it can be a .nix file for persistence, but accessible only
	// thru some shared state controller? Might it be stored in terraform
	// state provider?
	pub fn data(&self) -> MutexGuard<FleetData> {
		self.data.lock().unwrap()
	}
	pub fn data_mut(&self) -> MutexGuard<FleetData> {
		self.data.lock().unwrap()
	}
	pub fn save(&self) -> Result<()> {
		let mut tempfile = NamedTempFile::new_in(self.directory.clone()).context("failed to create updated version of fleet.nix in the same directory as original.\nDo you have write access to it? Access only to the fleet.nix won't be enough, the directory is used for atomic overwrite operation.\nIt is not recommended to use fleet by root anyway, move fleet project to your home directory.")?;
		let data = nixlike::serialize(&self.data() as &FleetData)?;
		tempfile.write_all(
			format!(
				"# This file contains fleet state and shouldn't be edited by hand\n\n{}\n\n# vim: ts=2 et nowrap\n",
				data
			)
			.as_bytes(),
		)?;
		let mut fleet_data_path = self.directory.clone();
		fleet_data_path.push("fleet.nix");
		tempfile.persist(fleet_data_path)?;
		Ok(())
	}
}
