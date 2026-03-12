use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::nix_raw::{derivation_free, derivation_to_json, store_drv_from_store_path};
use crate::{copy_nix_str, with_store_context};

fn store_dir() -> Result<String> {
	let mut out = String::new();
	with_store_context(|c, store, _| unsafe {
		crate::nix_raw::store_get_storedir(c, store, Some(copy_nix_str), (&raw mut out).cast())
	})?;
	Ok(out)
}

fn to_absolute_store_path(store_dir: &str, path: &str) -> String {
	if path.starts_with('/') {
		path.to_owned()
	} else {
		format!("{store_dir}/{path}")
	}
}

pub struct Derivation(*mut crate::nix_raw::derivation);
unsafe impl Send for Derivation {}

impl Derivation {
	pub fn from_path(drv_path: &str) -> Result<Self> {
		let path_c = CString::new(drv_path)?;
		let store_path = with_store_context(|c, store, _| unsafe {
			crate::nix_raw::store_parse_path(c, store, path_c.as_ptr())
		})?;
		let drv = with_store_context(|c, store, _| unsafe {
			store_drv_from_store_path(c, store, store_path)
		});
		unsafe { crate::nix_raw::store_path_free(store_path) };
		let drv = drv?;
		if drv.is_null() {
			bail!("failed to read derivation from {drv_path}");
		}
		Ok(Self(drv))
	}

	pub fn to_json_string(&self) -> Result<String> {
		let mut out = String::new();
		with_store_context(|c, _, _| unsafe {
			derivation_to_json(c, self.0, Some(copy_nix_str), (&raw mut out).cast())
		})?;
		Ok(out)
	}

	pub fn parsed(&self) -> Result<DrvParsed> {
		let s = self.to_json_string()?;
		Ok(serde_json::from_str(&s)?)
	}
}

impl Drop for Derivation {
	fn drop(&mut self) {
		unsafe { derivation_free(self.0) };
	}
}

#[derive(Debug, Deserialize)]
pub struct DrvParsed {
	pub inputs: DrvInputs,
	pub outputs: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct DrvInputs {
	#[serde(default)]
	pub srcs: Vec<String>,
	#[serde(default)]
	pub drvs: HashMap<String, DrvInputEntry>,
}

#[derive(Debug, Deserialize)]
pub struct DrvInputEntry {
	pub outputs: Vec<String>,
}

#[derive(Debug)]
pub struct DrvGraph {
	pub root: String,
	pub nodes: HashMap<String, DrvNode>,
}

#[derive(Debug)]
pub struct DrvNode {
	pub name: String,
	pub input_drvs: HashMap<String, Vec<String>>,
	pub input_srcs: Vec<String>,
	pub outputs: Vec<String>,
}

impl DrvGraph {
	pub fn resolve(drv_path: &str) -> Result<Self> {
		let sd = store_dir()?;
		let root = to_absolute_store_path(&sd, drv_path);

		let mut nodes = HashMap::new();
		let mut queue = VecDeque::new();
		let mut visited = HashSet::new();
		queue.push_back(root.clone());
		visited.insert(root.clone());

		while let Some(path) = queue.pop_front() {
			let drv = Derivation::from_path(&path)?;
			let parsed = drv.parsed()?;

			let input_drvs: HashMap<String, Vec<String>> = parsed
				.inputs
				.drvs
				.into_iter()
				.map(|(k, v)| (to_absolute_store_path(&sd, &k), v.outputs))
				.collect();

			for dep_path in input_drvs.keys() {
				if visited.insert(dep_path.clone()) {
					queue.push_back(dep_path.clone());
				}
			}

			nodes.insert(
				path.clone(),
				DrvNode {
					name: extract_drv_name(&path),
					input_drvs,
					input_srcs: parsed.inputs.srcs,
					outputs: parsed.outputs.into_keys().collect(),
				},
			);
		}

		Ok(Self { root, nodes })
	}
}

fn extract_drv_name(drv_path: &str) -> String {
	drv_path
		.rsplit('/')
		.next()
		.and_then(|f| f.strip_suffix(".drv"))
		.and_then(|f| f.split_once('-').map(|(_, name)| name))
		.unwrap_or(drv_path)
		.to_owned()
}
