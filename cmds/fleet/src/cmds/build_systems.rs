use std::{env::current_dir, os::unix::fs::symlink, path::PathBuf};

use anyhow::{anyhow, Result};
use clap::Parser;
use fleet_base::{
	deploy::{deploy_task, upload_task, DeployAction},
	host::{Config, DeployKind, GenerationStorage},
	opts::FleetOpts,
};
use nix_eval::{nix_go, NixBuildBatch};
use tokio::task::LocalSet;
use tracing::{error, field, info, info_span, warn, Instrument};

#[derive(Parser)]
pub struct Deploy {
	/// Disable automatic rollback
	#[clap(long)]
	disable_rollback: bool,
	/// Action to execute after system is built
	action: DeployAction,
}

#[derive(Parser, Clone)]
pub struct BuildSystems {
	/// Attribute to build. Systems are deployed from "toplevel" attr, well-known used attributes
	/// are "sdImage"/"isoImage", and your configuration may include any other build attributes.
	#[clap(long, default_value = "toplevel")]
	build_attr: String,
}

async fn build_task(
	config: Config,
	hostname: String,
	build_attr: &str,
	batch: Option<NixBuildBatch>,
) -> Result<PathBuf> {
	info!("building");
	let host = config.host(&hostname).await?;
	// let action = Action::from(self.subcommand.clone());
	let nixos = host.nixos_config().await?;
	let drv = nix_go!(nixos.system.build[{ build_attr }]);
	let outputs = drv.build_maybe_batch(batch).await?;
	let out_output = outputs
		.get("out")
		.ok_or_else(|| anyhow!("system build should produce \"out\" output"))?;

	// We already have system profiles for backups.
	if !host.local {
		info!("adding gc root");
		let mut cmd = config.local_host().cmd("nix").await?;
		cmd.arg("build")
			.comparg(
				"--profile",
				format!(
					"/nix/var/nix/profiles/{}-{hostname}",
					config.data().gc_root_prefix
				),
			)
			.arg(out_output);
		cmd.sudo().run_nix().await?;
	}

	Ok(out_output.clone())
}

impl BuildSystems {
	pub async fn run(self, config: &Config, opts: &FleetOpts) -> Result<()> {
		let hosts = opts.filter_skipped(config.list_hosts().await?).await?;
		let set = LocalSet::new();
		let build_attr = self.build_attr.clone();
		let batch = (hosts.len() > 1).then(|| {
			config
				.nix_session
				.new_build_batch("build-hosts".to_string())
		});
		for host in hosts {
			let config = config.clone();
			let span = info_span!("build", host = field::display(&host.name));
			let hostname = host.name;
			let build_attr = build_attr.clone();
			let batch = batch.clone();
			set.spawn_local(
				(async move {
					let built = match build_task(config, hostname.clone(), &build_attr, batch).await
					{
						Ok(path) => path,
						Err(e) => {
							error!("failed to deploy host: {}", e);
							return;
						}
					};
					// TODO: Handle error
					let mut out = current_dir().expect("cwd exists");
					out.push(format!("built-{}", hostname));

					info!("linking iso image to {:?}", out);
					if let Err(e) = symlink(built, out) {
						error!("failed to symlink: {e}")
					}
				})
				.instrument(span),
			);
		}
		drop(batch);
		set.await;
		Ok(())
	}
}

impl Deploy {
	pub async fn run(self, config: &Config, opts: &FleetOpts) -> Result<()> {
		let hosts = opts.filter_skipped(config.list_hosts().await?).await?;
		let set = LocalSet::new();
		let batch = (hosts.len() > 1).then(|| {
			config
				.nix_session
				.new_build_batch("deploy-hosts".to_string())
		});
		for host in hosts.into_iter() {
			let config = config.clone();
			let span = info_span!("deploy", host = field::display(&host.name));
			let hostname = host.name.clone();
			let opts = opts.clone();
			let batch = batch.clone();
			if let Some(deploy_kind) = opts.action_attr::<DeployKind>(&host, "deploy_kind").await? {
				host.set_deploy_kind(deploy_kind);
			};

			set.spawn_local(
				(async move {
					let built =
						match build_task(config.clone(), hostname.clone(), "toplevel", batch).await
						{
							Ok(path) => path,
							Err(e) => {
								error!("failed to build host system closure: {}", e);
								return;
							}
						};

					let deploy_kind = match host.deploy_kind().await {
						Ok(v) => v,
						Err(e) => {
							error!("failed to query target deploy kind: {e}");
							return;
						}
					};

					// TODO: Make disable_rollback a host attribute instead
					let mut disable_rollback = self.disable_rollback;
					if !disable_rollback && deploy_kind != DeployKind::Fleet {
						warn!("disabling rollback, as not supported by non-fleet deployment kinds");
						disable_rollback = true;
					}

					let remote_path =
						match upload_task(&config, &host, GenerationStorage::Deployer, built).await
						{
							Ok(v) => v,
							Err(e) => {
								error!("upload failed: {e}");
								return;
							}
						};

					if let Err(e) = deploy_task(
						self.action,
						&host,
						remote_path,
						if let Ok(v) = opts.action_attr(&host, "specialisation").await {
							v
						} else {
							error!("unreachable? failed to get specialization");
							return;
						},
						disable_rollback,
					)
					.await
					{
						error!("activation failed: {e}");
					}
				})
				.instrument(span),
			);
		}
		drop(batch);
		set.await;
		Ok(())
	}
}
