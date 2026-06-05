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
	pub outputs: HashMap<String, DrvParsedOutput>,
}

#[derive(Debug, Deserialize)]
pub struct DrvParsedOutput {
	#[serde(default)]
	pub path: Option<String>,
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

#[derive(Debug, Clone)]
pub struct DrvGraph {
	pub root: String,
	pub nodes: HashMap<String, DrvNode>,
}

#[derive(Debug, Clone)]
pub struct DrvNode {
	pub name: String,
	pub input_drvs: HashMap<String, Vec<String>>,
	pub input_srcs: Vec<String>,
	// TODO: CA outputs without a known paths are skipped
	pub outputs: HashMap<String, String>,
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

			let outputs: HashMap<String, String> = parsed
				.outputs
				.into_iter()
				.filter_map(|(name, out)| out.path.map(|p| (name, to_absolute_store_path(&sd, &p))))
				.collect();

			nodes.insert(
				path.clone(),
				DrvNode {
					name: extract_drv_name(&path),
					input_drvs,
					input_srcs: parsed.inputs.srcs,
					outputs,
				},
			);
		}

		Ok(Self { root, nodes })
	}

	pub fn wanted_outputs(&self, root_outputs: &[String]) -> HashMap<String, Vec<String>> {
		let mut wanted: HashMap<String, HashSet<String>> = HashMap::new();
		wanted.insert(self.root.clone(), root_outputs.iter().cloned().collect());

		let mut queue: VecDeque<String> = VecDeque::new();
		queue.push_back(self.root.clone());
		while let Some(path) = queue.pop_front() {
			let Some(node) = self.nodes.get(&path) else {
				continue;
			};
			for (dep_path, dep_outputs) in &node.input_drvs {
				let entry = wanted.entry(dep_path.clone()).or_default();
				let mut changed = false;
				for o in dep_outputs {
					if entry.insert(o.clone()) {
						changed = true;
					}
				}
				if changed {
					queue.push_back(dep_path.clone());
				}
			}
		}

		wanted
			.into_iter()
			.map(|(k, v)| {
				let mut v: Vec<_> = v.into_iter().collect();
				v.sort();
				(k, v)
			})
			.collect()
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
