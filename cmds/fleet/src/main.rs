#![recursion_limit = "512"]

pub(crate) mod cmds;
// pub(crate) mod command;
pub(crate) mod extra_args;

use std::{ffi::OsString, process::ExitCode};

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser};
use cmds::{
	build_systems::{BuildSystems, Deploy},
	complete::Complete,
	info::Info,
	rollback::RollbackSingle,
	secrets::Secret,
	tf::Tf,
};
use fleet_base::{host::Config, opts::FleetOpts};
use futures::{TryStreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
// use host::Config;
#[cfg(feature = "indicatif")]
use human_repr::HumanCount;
#[cfg(feature = "indicatif")]
use indicatif::{ProgressState, ProgressStyle};
use nix_eval::{gc_register_my_thread, gc_unregister_my_thread, init_libraries};
use tracing::{Instrument, error, info, info_span};
#[cfg(feature = "indicatif")]
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{fmt::format::Format, prelude::*, EnvFilter};

#[derive(Parser)]
struct Prefetch {}
impl Prefetch {
	async fn run(&self, config: &Config) -> Result<()> {
		let mut prefetch_dir = config.directory.to_path_buf();
		prefetch_dir.push("prefetch");
		if !prefetch_dir.is_dir() {
			info!("nothing to prefetch: no prefetch directory");
			return Ok(());
		}
		let tasks = <FuturesUnordered<LocalBoxFuture<Result<()>>>>::new();
		for entry in std::fs::read_dir(&prefetch_dir)? {
			tasks.push(Box::pin(async {
				let entry = entry?;
				if !entry.metadata()?.is_file() {
					bail!("only files should exist in prefetch directory");
				}
				let span = info_span!(
					"prefetching",
					name = entry.file_name().to_string_lossy().as_ref()
				);
				let mut path = OsString::new();
				path.push("file://");
				path.push(entry.path());

				let mut status = config.local_host().cmd("nix").await?;
				status.args(&config.nix_args);
				status.arg("store").arg("prefetch-file").arg(path);
				status.run_nix_string().instrument(span).await?;
				Ok(())
			}));
		}
		tasks.try_collect::<Vec<()>>().await?;
		Ok(())
	}
}

#[derive(Parser)]
enum Opts {
	/// Build system closures
	BuildSystems(BuildSystems),
	/// Upload and switch system closures
	Deploy(Deploy),
	/// Rollback remote machine by redeploying old generation as the new one
	RollbackSingle(RollbackSingle),
	/// Secret management
	#[clap(subcommand)]
	Secret(Secret),
	/// Upload prefetch directory to the nix store
	Prefetch(Prefetch),
	/// Config parsing
	Info(Info),
	/// Command completions
	#[clap(hide(true))]
	Complete(Complete),
	/// Compile and evaluate terranix configuration
	Tf(Tf),
}

#[derive(Parser)]
#[clap(version, author)]
struct RootOpts {
	#[clap(flatten)]
	fleet_opts: FleetOpts,
	#[clap(subcommand)]
	command: Opts,
}

async fn run_command(config: &Config, opts: FleetOpts, command: Opts) -> Result<()> {
	match command {
		Opts::BuildSystems(c) => c.run(config, &opts).await?,
		Opts::Deploy(d) => d.run(config, &opts).await?,
		Opts::RollbackSingle(r) => r.run(config, &opts).await?,
		Opts::Secret(s) => s.run(config, &opts).await?,
		Opts::Info(i) => i.run(config).await?,
		Opts::Prefetch(p) => p.run(config).await?,
		Opts::Tf(t) => t.run(config).await?,
		// TODO: actually parse commands before starting the async runtime
		Opts::Complete(c) => {
			tokio::task::spawn_blocking(move || c.run(RootOpts::command())).await?
		}
	};
	Ok(())
}

fn setup_logging() {
	#[cfg(feature = "indicatif")]
	let indicatif_layer = {
		use std::time::Duration;

		IndicatifLayer::new().with_progress_style(
			ProgressStyle::with_template(
				"{color_start}{span_child_prefix} {span_name}{{{span_fields}}}{color_end} {wide_msg} {color_start}{download_progress} {elapsed}{color_end}",
			)
				.unwrap()
				.with_key("download_progress", |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
					let Some(len) = state.len() else {
						return;
					};
					let pos = state.pos();
					if pos > len {
						let _ = write!(writer, "{}", pos.human_count_bare());
					} else {
						let _ = write!(writer, "{} / {}", pos.human_count_bare(), len.human_count_bare());
					}
				})
				.with_key(
					"color_start",
					|state: &ProgressState, writer: &mut dyn std::fmt::Write| {
						let elapsed = state.elapsed();

						if elapsed > Duration::from_secs(60) {
							// Red
							let _ = write!(writer, "\x1b[{}m", 1 + 30);
						} else if elapsed > Duration::from_secs(30) {
							// Yellow
							let _ = write!(writer, "\x1b[{}m", 3 + 30);
						}
					},
				)
				.with_key(
					"color_end",
					|state: &ProgressState, writer: &mut dyn std::fmt::Write| {
						if state.elapsed() > Duration::from_secs(30) {
							let _ = write!(writer, "\x1b[0m");
						}
					},
				),
		)
	};

	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

	let reg = tracing_subscriber::registry().with({
		let sub = tracing_subscriber::fmt::layer()
			.without_time()
			.with_target(false);
		#[cfg(feature = "indicatif")]
		let sub = sub.with_writer(indicatif_layer.get_stderr_writer());
		sub.with_filter(filter) // .without,
	});
	// #[cfg(feature = "indicatif")]
	#[cfg(feature = "indicatif")]
	let reg = reg.with(indicatif_layer);
	reg.init();
}

fn main() -> ExitCode {
	let opts = RootOpts::parse();
	if let Opts::Complete(c) = &opts.command {
		c.run(RootOpts::command());
		return ExitCode::SUCCESS;
	}

	setup_logging();

	init_libraries();

	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.on_thread_start(|| {
			gc_register_my_thread();
		})
		.on_thread_stop(|| {
			gc_unregister_my_thread();
		})
		.build()
		.expect("failed to build runtime")
		.block_on(async {
			if let Err(e) = main_real(opts).await {
				error!("{e:#}");
				ExitCode::FAILURE
			} else {
				ExitCode::SUCCESS
			}
		})
	// async_main(opts)
}

async fn main_real(opts: RootOpts) -> Result<()> {
	let nix_args = std::env::var_os("NIX_ARGS")
		.map(|a| extra_args::parse_os(&a))
		.transpose()?
		.unwrap_or_default();
	let config = opts
		.fleet_opts
		.build(
			nix_args,
			matches!(opts.command, Opts::Deploy(_) | Opts::BuildSystems(_)),
		)
		.await?;

	match run_command(&config, opts.fleet_opts, opts.command).await {
		Ok(()) => {
			config.save()?;
			Ok(())
		}
		Err(e) => {
			let _ = config.save();
			Err(e)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn verify_command() {
		use clap::CommandFactory;
		RootOpts::command().debug_assert();
	}
}
