use std::collections::HashSet;

use anyhow::{Result, bail};
use clap::Parser;
use fleet_base::{
	deploy::{DeployAction, deploy_task, upload_task},
	host::{Config, ConfigHost, Generation, GenerationStorage},
	opts::FleetOpts,
};
use tabled::Table;
use tracing::{info, warn};

#[derive(Parser)]
pub struct RollbackSingle {
	machine: String,
	#[clap(subcommand)]
	action: RollbackAction,
}

#[derive(Parser, Clone)]
struct DeployOptions {
	/// Rollback target to use
	id: String,
	/// Rollback to the current generation if rollback fails
	// Automatic rollback seems to be unnecessary for manual rollback...
	#[clap(long)]
	enable_rollback: bool,
	/// Specialization to use
	#[clap(long)]
	specialization: Option<String>,
}

#[derive(Parser, Clone)]
enum RollbackAction {
	/// List available rollback targets
	ListTargets,
	/// Upload and execute the activation script, old version will be used after reboot.
	Test(#[clap(flatten)] DeployOptions),
	/// Upload, set current profile, and execute activation script.
	Switch(#[clap(flatten)] DeployOptions),
	/// Upload and set as current system profile, but do not execute activation script.
	Boot(#[clap(flatten)] DeployOptions),
}

pub async fn list_all_generations(host: &ConfigHost, config: &Config) -> Vec<Generation> {
	let stored_on_machine = host
		.list_generations("system")
		.await
		.inspect_err(|e| {
			warn!("failed to list generations available on the remote machine: {e}");
		})
		.unwrap_or_default();
	let on_machine_store_paths = stored_on_machine
		.iter()
		.map(|g| &g.store_path)
		.collect::<HashSet<_>>();
	let mut stored_locally = config
		.local_host()
		.list_generations(&format!("{}-{}", config.data().gc_root_prefix, host.name))
		.await
		.inspect_err(|e| {
			warn!("failed to list generations available locally: {e}");
		})
		.unwrap_or_default();
	dbg!(&stored_locally);
	stored_locally.retain(|g| !on_machine_store_paths.contains(&g.store_path));
	for ele in stored_locally.iter_mut() {
		ele.current = false;
		ele.location = GenerationStorage::Deployer;
	}
	stored_locally.extend(stored_on_machine);
	stored_locally.sort_by_key(|v| v.datetime);
	stored_locally
}

impl RollbackSingle {
	pub(crate) async fn run(&self, config: &Config, _opts: &FleetOpts) -> Result<()> {
		let host = config.host(&self.machine).await?;
		match &self.action {
			RollbackAction::ListTargets => {
				let generations = list_all_generations(&host, config).await;
				if generations.is_empty() {
					bail!("no available rollback targets found");
				}
				info!("Generation list:\n{}", Table::new(&generations));
				Ok(())
			}
			RollbackAction::Boot(o) | RollbackAction::Test(o) | RollbackAction::Switch(o) => {
				let DeployOptions {
					id,
					enable_rollback,
					specialization,
				} = o;
				let action: DeployAction = match self.action {
					RollbackAction::Test { .. } => DeployAction::Test,
					RollbackAction::Switch { .. } => DeployAction::Switch,
					RollbackAction::Boot { .. } => DeployAction::Boot,
					_ => unreachable!(),
				};
				let generations = list_all_generations(&host, config).await;
				let Some(generation) = generations.iter().find(|g| &g.rollback_id() == id) else {
					bail!(
						"generation by this name is not found, existing generations:\n{}",
						Table::new(&generations)
					);
				};
				let remote_path = upload_task(
					config,
					&host,
					generation.location,
					generation.store_path.clone(),
				)
				.await?;

				deploy_task(
					action,
					&host,
					remote_path,
					specialization.clone(),
					!*enable_rollback,
				)
				.await?;
				Ok(())
			}
		}
	}
}
