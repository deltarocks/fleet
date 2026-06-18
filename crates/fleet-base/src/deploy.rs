use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use camino::Utf8PathBuf;
use clap::ValueEnum;
use itertools::Itertools;
use remowt_endpoints::fs::FsClient;
use remowt_endpoints::systemd::SystemdClient;
use remowt_fleet::NixClient;
use remowt_link_shared::BifConfig;
use tokio::time::sleep;
use tracing::{Instrument as _, error, info, info_span, warn};

use crate::host::{ConfigHost, DeployKind};
use crate::pins::{Generation, GenerationStorage};

#[derive(ValueEnum, Clone, Copy)]
pub enum DeployAction {
	/// Upload derivation, but do not execute the update.
	Upload,
	/// Upload and execute the activation script, old version will be used after reboot.
	Test,
	/// Upload and set as current system profile, but do not execute activation script.
	Boot,
	/// Upload, set current profile, and execute activation script.
	Switch,
}

impl DeployAction {
	pub(crate) fn name(&self) -> Option<&'static str> {
		match self {
			Self::Upload => None,
			Self::Test => Some("test"),
			Self::Boot => Some("boot"),
			Self::Switch => Some("switch"),
		}
	}
	pub(crate) fn should_switch_profile(&self) -> bool {
		matches!(self, Self::Switch | Self::Boot)
	}
	pub(crate) fn should_activate(&self) -> bool {
		matches!(self, Self::Switch | Self::Test | Self::Boot)
	}
	pub(crate) fn should_create_rollback_marker(&self) -> bool {
		// Upload does nothing on the target machine, other than uploading the closure.
		// In boot case we want to have rollback marker prepared, so that the system may rollback itself on the next boot.
		!matches!(self, Self::Upload)
	}
	pub(crate) fn should_schedule_rollback_run(&self) -> bool {
		matches!(self, Self::Switch | Self::Test)
	}
}

async fn get_current_generation(host: &ConfigHost) -> Result<Generation> {
	let generations = host.list_generations("system").await?;
	let current = generations
		.into_iter()
		.filter(|g| g.current)
		.at_most_one()
		.map_err(|_e| anyhow!("bad list-generations output"))?
		.ok_or_else(|| anyhow!("failed to find generation"))?;
	Ok(current)
}

pub async fn deploy_task(
	action: DeployAction,
	host: &ConfigHost,
	built: Utf8PathBuf,
	specialisation: Option<String>,
	disable_rollback: bool,
) -> Result<()> {
	let remowt = host.remowt().await?;
	let deploy_kind = host.deploy_kind().await?;
	if (deploy_kind == DeployKind::NixosInstall || deploy_kind == DeployKind::NixosLustrate)
		&& !matches!(action, DeployAction::Boot | DeployAction::Upload)
	{
		bail!("{deploy_kind:?} deploy kind only supports boot and upload actions");
	}

	let mut failed = false;

	// TODO: Lockfile, to prevent concurrent system switch?
	// TODO: If rollback target exists - bail, it should be removed. Lockfile will not work in case if rollback
	// is scheduler on next boot (default behavior). On current boot - rollback activator will fail due to
	// unit name conflict in systemd-run
	// This code is tied to rollback.nix
	if !disable_rollback && action.should_create_rollback_marker() {
		// let _span = info_span!("preparing").entered();
		info!("preparing for rollback");
		let generation = get_current_generation(host).await?;
		info!(
			"rollback target would be {} {}",
			generation.id, generation.datetime
		);
		{
			let mut cmd = remowt.cmd("sh");
			cmd.arg("-c").arg(format!("mark=$(mktemp -p /etc -t fleet_rollback_marker.XXXXX) && echo -n {} > $mark && mv --no-clobber $mark /etc/fleet_rollback_marker", generation.id));
			if let Err(e) = cmd.sudo().run().await {
				error!("failed to set rollback marker: {e}");
				failed = true;
			}
		}
		// Activation script also starts rollback-watchdog.timer, however, it is possible that it won't be started.
		// Kicking it on manually will work best.
		//
		// There wouldn't be conflict, because here we trigger start of the primary service, and systemd will
		// only allow one instance of it.

		// TODO: We should also watch how this process is going.
		// After running this command, we have less than 3 minutes to deploy everything,
		// if we fail to perform generation switch in time, then we will still call the activation script, and this may break something.
		// Anyway, reboot will still help in this case.
		if action.should_schedule_rollback_run() {
			let mut cmd = remowt.cmd("systemd-run");
			cmd.comparg("--on-active", "3min")
				.comparg("--unit", "rollback-watchdog-run")
				.arg("systemctl")
				.arg("start")
				.arg("rollback-watchdog.service");
			if let Err(e) = cmd.sudo().run().await {
				error!("failed to schedule rollback run: {e}");
				failed = true;
			}
		}
	}

	let remowt = host.remowt().await?;
	let fs = remowt.endpoints::<FsClient<_>>();
	if deploy_kind == DeployKind::NixosLustrate {
		// Fleet could also create this file, but as this operation is potentially disruptive,
		// make user do it themself.
		if !fs
			.file_exists(Utf8PathBuf::from("/etc/NIXOS_LUSTRATE"))
			.await?
		{
			bail!("/etc/NIXOS_LUSTRATE should be created on remote host");
		}
		// Wanted by NixOS to recognize the system as NixOS.
		let mut cmd = remowt.cmd("touch");
		cmd.arg("/etc/NIXOS");
		cmd.sudo().run().await.context("creating /etc/NIXOS")?;
	}
	if deploy_kind == DeployKind::NixosInstall {
		info!(
			"running nixos-install to switch profile, install bootloader, and perform activation"
		);
		let mut cmd = remowt.cmd("nixos-install");
		cmd.arg("--system").arg(&built).args([
			// Channels here aren't fleet host system channels, but channels embedded in installation cd, which might be old.
			// It is possible to copy host channels, but I would prefer non-flake nix just to be unsupported.
			"--no-channel-copy",
			"--root",
			"/mnt",
		]);
		if let Err(e) = cmd.sudo().run().await {
			error!("failed to execute nixos-install: {e}");
			failed = true;
		}
	} else {
		if action.should_switch_profile() && !failed {
			info!("switching system profile generation");

			match host.ensure_nix_plugin().await {
				Ok(plugin_id) => {
					let nix_elevated = remowt.plugin_endpoints::<NixClient<_>>(plugin_id);
					// To avoid even more problems, using nixos-install for now.
					// // nix build is unable to work with --store argument for some reason, and nix until 2.26 didn't support copy with --profile argument,
					// // falling back to using nix-env command
					// // After stable NixOS starts using 2.26 - use `nix --store /mnt copy --from /mnt --profile ...` here, and instead of nix build below.
					// let mut cmd = host.cmd("nix-env").await?;
					// cmd.args([
					// 	"--store",
					// 	"/mnt",
					// 	"--profile",
					// 	"/mnt/nix/var/nix/profiles/system",
					// 	"--set",
					// ])
					// .arg(&built);
					// if let Err(e) = cmd.sudo().run_nix().await {
					// 	error!("failed to switch system profile generation: {e}");
					// 	failed = true;
					// }
					if let Err(e) = nix_elevated
						.switch_profile("/nix/var/nix/profiles/system".to_owned(), built.clone())
						.await
					{
						error!("failed to switch system profile generation: {e}");
						failed = true;
					}
				}
				Err(e) => {
					failed = true;
					error!("failed to enable nix plugin: {e:?}");
				}
			}
		}

		// FIXME: Connection might be disconnected after activation run

		if action.should_activate() && !failed {
			// let _span = info_span!("activating").entered();
			info!("executing activation script");
			let specialised = if let Some(specialisation) = specialisation {
				let mut specialised = built.join("specialisation");
				specialised.push(specialisation);
				specialised
			} else {
				built.clone()
			};
			let switch_script = specialised.join("bin/switch-to-configuration");
			let mut cmd = remowt.cmd("systemd-run");
			if deploy_kind == DeployKind::NixosLustrate {
				cmd.arg("--setenv=NIXOS_INSTALL_BOOTLOADER=1");
			}
			cmd.arg("--setenv=FLEET_ONLINE_ACTIVATION=1")
				.arg("--collect")
				.arg("--no-ask-password")
				.arg("--pipe")
				.arg("--quiet")
				.arg("--service-type=exec")
				.arg("--unit=fleet-switch-to-configuration")
				.arg(switch_script)
				.arg(action.name().expect("upload.should_activate == false"));
			if let Err(e) = cmd.sudo().run().in_current_span().await {
				error!("failed to activate: {e}");
				failed = true;
			}
		}
	}
	if action.should_create_rollback_marker() {
		let elevated_systemd = remowt.run0_endpoints::<SystemdClient<BifConfig>>().await?;
		let elevated_fs = remowt.run0_endpoints::<FsClient<BifConfig>>().await?;
		if !disable_rollback {
			if failed {
				if action.should_schedule_rollback_run() {
					info!("executing rollback");
					if let Err(e) = elevated_systemd
						.start("rollback-watchdog.service".to_owned())
						.instrument(info_span!("rollback"))
						.await
					{
						error!("failed to trigger rollback: {e}")
					}
				}
			} else {
				info!("trying to mark upgrade as successful");
				if let Err(e) = elevated_fs
					.rm_file(Utf8PathBuf::from("/etc/fleet_rollback_marker"))
					.in_current_span()
					.await
				{
					error!(
						"failed to remove rollback marker. This is bad, as the system will be rolled back by watchdog: {e}"
					)
				}
			}
			info!("disarming watchdog, just in case");
			if let Err(_e) = elevated_systemd
				.stop("rollback-watchdog.timer".to_owned())
				.await
			{
				// It is ok, if there was no reboot - then timer might not be running.
			}
			if action.should_schedule_rollback_run()
				&& let Err(e) = elevated_systemd
					.stop("rollback-watchdog-run.timer".to_owned())
					.await
			{
				error!("failed to disarm rollback run: {e}");
			}
		} else if let Err(_e) = elevated_fs
			.rm_file(Utf8PathBuf::from("/etc/fleet_rollback_marker"))
			.in_current_span()
			.await
		{
			// Marker might not exist, yet better try to remove it.
		}
	}
	Ok(())
}

pub async fn upload_task(
	host: &ConfigHost,
	location: GenerationStorage,
	generation: Utf8PathBuf,
) -> Result<Utf8PathBuf> {
	if matches!(location, GenerationStorage::Pusher) {
		bail!("pusher is not enabled in this version of fleet");
	}
	if !host.local {
		info!("uploading system closure");
		let mut tries = 0;
		loop {
			match host.remote_derivation(&generation).await {
				Ok(remote) => {
					assert!(remote == generation, "CA derivations aren't implemented");
					return Ok(remote);
				}
				Err(e) if tries < 3 => {
					tries += 1;
					warn!("copy failure ({}/3): {:#}", tries, e);
					sleep(Duration::from_millis(5000)).await;
				}
				Err(e) => {
					bail!("upload failed: {e:#}");
				}
			}
		}
	}
	Ok(generation)
}
