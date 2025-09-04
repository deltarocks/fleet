use std::{
	collections::BTreeMap,
	env::current_dir,
	ffi::OsString,
	str::FromStr,
	sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use nix_eval::{
	FetchSettings, FlakeReference, FlakeReferenceParseFlags, FlakeSettings, Value, nix_go,
	util::assert_warn,
};
use nom::{
	Parser,
	bytes::complete::take_while1,
	character::complete::char,
	combinator::{map, opt},
	multi::separated_list1,
	sequence::{preceded, separated_pair},
};

use crate::{
	fleetdata::FleetData,
	host::{Config, ConfigHost, FleetConfigInternals},
};

#[derive(Clone)]
pub enum HostItem {
	Host {
		name: String,
		attrs: BTreeMap<String, String>,
	},
	Tag {
		name: String,
		attrs: BTreeMap<String, String>,
	},
}
fn host_item_parser(input: &str) -> Result<HostItem, String> {
	fn err_to_string(err: nom::Err<nom::error::Error<&str>>) -> String {
		err.to_string()
	}

	let (input, is_tag) = map(opt(char('@')), |c| c.is_some())
		.parse_complete(input)
		.map_err(err_to_string)?;
	let (input, name) = map(
		take_while1(|v| v != ',' && v != '?' && v != '@'),
		str::to_owned,
	)
	.parse_complete(input)
	.map_err(err_to_string)?;

	let kw_item = separated_pair(
		map(take_while1(|v| v != '&' && v != '='), str::to_owned),
		char('='),
		map(take_while1(|v| v != '&'), str::to_owned),
	);
	let kw = map(separated_list1(char('&'), kw_item), |vec| {
		vec.into_iter().collect::<BTreeMap<_, _>>()
	});
	let mut opt_kw = map(opt(preceded(char('?'), kw)), Option::unwrap_or_default);

	let (input, attrs) = opt_kw.parse_complete(input).map_err(err_to_string)?;

	if !input.is_empty() {
		return Err(format!("unexpected trailing input: {input:?}"));
	}
	Ok(if is_tag {
		HostItem::Tag { name, attrs }
	} else {
		HostItem::Host { name, attrs }
	})
}

// TODO: Rename to HostSelector
#[derive(clap::Parser, Clone)]
pub struct FleetOpts {
	/// All hosts except those would be skipped
	#[clap(long, number_of_values = 1, value_parser = host_item_parser)]
	pub only: Vec<HostItem>,

	/// Hosts to skip
	#[clap(long, number_of_values = 1)]
	pub skip: Vec<String>,

	/// Host, which should be threaten as current machine
	// TODO: Replace with connectivity refactor
	#[clap(long, default_value_t = hostname::get().expect("unknown hostname").to_str().expect("hostname is not utf-8").to_owned())]
	pub localhost: String,

	/// Override detected system for host, to perform builds via
	/// binfmt-declared qemu instead of trying to crosscompile
	#[clap(long, default_value = env!("NIX_SYSTEM"))]
	pub local_system: String,

	/// By default fleet continues on single derivation build failure
	/// this flag makes command fail immediately
	///
	/// Opposite of Nix's --keep-going
	#[clap(long)]
	pub fail_fast: bool,
}

impl FleetOpts {
	pub async fn filter_skipped(
		&self,
		hosts: impl IntoIterator<Item = ConfigHost>,
	) -> Result<Vec<ConfigHost>> {
		let mut out = Vec::new();
		for host in hosts {
			if self.should_skip(&host).await? {
				continue;
			}
			out.push(host);
		}
		Ok(out)
	}
	pub async fn should_skip(&self, host: &ConfigHost) -> Result<bool> {
		if self.skip.iter().any(|h| h as &str == host.name) {
			return Ok(true);
		}
		if self.only.is_empty() {
			return Ok(false);
		}
		let mut have_group_matches = false;
		for item in self.only.iter() {
			match item {
				HostItem::Host { name, .. } if *name == host.name => {
					return Ok(false);
				}
				HostItem::Tag { .. } => {
					have_group_matches = true;
				}
				_ => {}
			}
		}
		if have_group_matches {
			let host_tags = host.tags().await?;
			for item in self.only.iter() {
				match item {
					HostItem::Tag { name, .. } if host_tags.contains(name) => {
						return Ok(false);
					}
					_ => {}
				}
			}
		}
		Ok(true)
	}
	pub async fn action_attr<T: FromStr>(&self, host: &ConfigHost, attr: &str) -> Result<Option<T>>
	where
		T::Err: Sync,
		anyhow::Error: From<T::Err>,
	{
		let str = self.action_attr_str(host, attr).await?;
		Ok(str.map(|v| T::from_str(&v)).transpose()?)
	}
	pub async fn action_attr_str(&self, host: &ConfigHost, attr: &str) -> Result<Option<String>> {
		if self.only.is_empty() {
			return Ok(None);
		}
		let mut have_group_matches = false;
		for item in self.only.iter() {
			match item {
				HostItem::Host { name, attrs }
					if *name == host.name && attrs.contains_key(attr) =>
				{
					return Ok(attrs.get(attr).cloned());
				}
				HostItem::Tag { attrs, .. } if attrs.contains_key(attr) => {
					have_group_matches = true;
				}
				_ => {}
			}
		}
		if have_group_matches {
			let host_tags = host.tags().await?;
			for item in self.only.iter() {
				match item {
					HostItem::Tag { name, attrs }
						if host_tags.contains(name) && attrs.contains_key(attr) =>
					{
						return Ok(attrs.get(attr).cloned());
					}
					_ => {}
				}
			}
		}
		Ok(None)
	}
	pub fn is_local(&self, host: &str) -> bool {
		self.localhost == host
	}

	// TODO: Config should be detached from opts.
	pub async fn build(&self, nix_args: Vec<OsString>, assert: bool) -> Result<Config> {
		let cwd = current_dir()?;
		let mut directory = cwd.clone();
		let mut fleet_data_path = directory.join("fleet.nix");
		while !fleet_data_path.is_file() {
			// fleet.nix
			fleet_data_path.pop();
			if !directory.pop() || !fleet_data_path.pop() {
				bail!(
					"fleet.nix not found at {} or any of the parent directories",
					cwd.display()
				);
			}
			fleet_data_path.push("fleet.nix");
		}
		let bytes =
			std::fs::read_to_string(&fleet_data_path).context("reading fleet state (fleet.nix)")?;
		let data: Mutex<FleetData> = nixlike::parse_str(&bytes)?;

		let mut fetch_settings = FetchSettings::new();
		fetch_settings.set(c"warn-dirty", c"false");

		let mut flake_settings = FlakeSettings::new()?;
		let mut parse = FlakeReferenceParseFlags::new(&flake_settings)?;
		// For some reason, lazy trees not being used when there is no base dir set
		parse.set_base_dir("/")?;

		let (mut flake, _) = FlakeReference::new(
			directory
				.to_str()
				.ok_or_else(|| anyhow::anyhow!("fleet dir should have utf-8 path"))?,
			&flake_settings,
			&parse,
			&fetch_settings,
		)?;
		let flake = flake.lock(&fetch_settings)?;

		let flake = flake.get_attrs(&mut flake_settings)?;

		let builtins_field = Value::eval("builtins")?;

		let fleet_root = flake.get_field("fleetConfigurations")?;
		let data_val = Value::serialized(&data)?;
		let fleet_field = nix_go!(fleet_root.default(data_val));

		let config_field = nix_go!(fleet_field.config);

		if assert {
			assert_warn("fleet config evaluation", &config_field)
				.await
				.context("failed to verify assertions")?;
		}

		let import = nix_go!(builtins_field.import);
		let overlays = nix_go!(config_field.nixpkgs.overlays);
		let nixpkgs = nix_go!(config_field.nixpkgs.buildUsing);
		let nixpkgs_imported = nix_go!(import(nixpkgs));

		let default_pkgs = nix_go!(nixpkgs_imported(Obj {
			overlays,
			system: self.local_system.clone(),
		}));

		Ok(Config(Arc::new(FleetConfigInternals {
			directory,
			data,
			local_system: self.local_system.clone(),
			nix_args,
			config_field,
			default_pkgs,
			nixpkgs,
			localhost: self.localhost.to_owned(),
		})))
	}
}
