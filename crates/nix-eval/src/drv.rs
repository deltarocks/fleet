use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Result, bail};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::nix_raw::{derivation_free, derivation_to_json, store_drv_from_store_path};
use crate::{Store, copy_nix_str, with_default_context};

pub struct Derivation(*mut crate::nix_raw::derivation);
unsafe impl Send for Derivation {}

impl Derivation {
	pub fn from_path(store: &Store, drv_path: &Utf8Path) -> Result<Self> {
		let store_path = store.parse_path(drv_path)?;
		let drv = with_default_context(|c, _| unsafe {
			store_drv_from_store_path(c, store.as_ptr(), store_path.as_ptr())
		});
		let drv = drv?;
		if drv.is_null() {
			bail!("failed to read derivation from {drv_path}");
		}
		Ok(Self(drv))
	}

	pub fn to_json_string(&self) -> Result<String> {
		let mut out = String::new();
		with_default_context(|c, _| unsafe {
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
	pub srcs: Vec<Utf8PathBuf>,
	#[serde(default)]
	pub drvs: HashMap<Utf8PathBuf, DrvInputEntry>,
}

#[derive(Debug, Deserialize)]
pub struct DrvInputEntry {
	pub outputs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DrvGraph {
	pub root: Utf8PathBuf,
	pub nodes: HashMap<Utf8PathBuf, DrvNode>,
}

#[derive(Debug, Clone)]
pub struct DrvNode {
	pub name: String,
	pub input_drvs: HashMap<Utf8PathBuf, Vec<String>>,
	pub input_srcs: Vec<Utf8PathBuf>,
	// TODO: CA outputs without a known paths are skipped
	pub outputs: HashMap<String, Utf8PathBuf>,
}

impl DrvGraph {
	pub fn resolve(store: &Store, drv_path: &Utf8Path) -> Result<Self> {
		let sd = store.store_dir()?;
		let root = sd.join(drv_path);

		let mut nodes = HashMap::new();
		let mut queue = VecDeque::new();
		let mut visited = HashSet::new();
		queue.push_back(root.clone());
		visited.insert(root.clone());

		while let Some(path) = queue.pop_front() {
			let drv = Derivation::from_path(store, &path)?;
			let parsed = drv.parsed()?;

			let input_drvs: HashMap<Utf8PathBuf, Vec<String>> = parsed
				.inputs
				.drvs
				.into_iter()
				.map(|(k, v)| (sd.join(&k), v.outputs))
				.collect();
			let input_srcs: Vec<Utf8PathBuf> = parsed
				.inputs
				.srcs
				.into_iter()
				.map(|k| sd.join(&k))
				.collect();

			for dep_path in input_drvs.keys() {
				if visited.insert(dep_path.clone()) {
					queue.push_back(dep_path.clone());
				}
			}

			let outputs: HashMap<String, Utf8PathBuf> = parsed
				.outputs
				.into_iter()
				.filter_map(|(name, out)| out.path.map(|p| (name, sd.join(&p))))
				.collect();

			nodes.insert(
				path.clone(),
				DrvNode {
					name: extract_drv_name(&path),
					input_drvs,
					input_srcs,
					outputs,
				},
			);
		}

		Ok(Self { root, nodes })
	}

	pub fn wanted_outputs(&self, root_outputs: &[String]) -> HashMap<Utf8PathBuf, Vec<String>> {
		let mut wanted: HashMap<Utf8PathBuf, HashSet<String>> = HashMap::new();
		wanted.insert(self.root.clone(), root_outputs.iter().cloned().collect());

		let mut queue: VecDeque<Utf8PathBuf> = VecDeque::new();
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

pub fn extract_drv_name(drv_path: &Utf8Path) -> String {
	let comp = drv_path
		.components()
		.rev()
		.next()
		.expect("drv path is at least one component");
	let Utf8Component::Normal(n) = comp else {
		panic!("drv path is normal");
	};

	let n = n.strip_suffix(".drv").unwrap_or(n);

	let n = n.split_once(' ').map(|(_, n)| n).unwrap_or(n);

	n.to_owned()
}
