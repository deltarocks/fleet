use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Semaphore, broadcast};
use tracing::{debug, info, instrument, warn};

use crate::drv::DrvGraph;

#[derive(Clone, Debug)]
pub enum BuildEvent {
	SubstitutePrepassStarted {
		paths: usize,
	},
	SubstitutePrepassFinished {
		satisfied: usize,
	},
	DrvStarted {
		drv_path: String,
		name: String,
		wanted: Vec<String>,
	},
	DrvSkipped {
		drv_path: String,
		name: String,
	},
	DrvFinished {
		drv_path: String,
		name: String,
	},
	DrvFailed {
		drv_path: String,
		name: String,
		error: String,
	},
	DrvCancelled {
		drv_path: String,
		name: String,
		failed_dep: String,
	},
}

pub struct Scheduler {
	parallelism: usize,
	events: broadcast::Sender<BuildEvent>,
}

impl Scheduler {
	pub fn new(parallelism: usize) -> Self {
		let parallelism = parallelism.max(1);
		let (events, _) = broadcast::channel(1024);
		Self {
			parallelism,
			events,
		}
	}

	pub fn subscribe(&self) -> broadcast::Receiver<BuildEvent> {
		self.events.subscribe()
	}

	#[instrument(name = "scheduler", skip(self, graph), fields(root = %graph.root, nodes = graph.nodes.len()))]
	pub async fn run(&self, graph: Arc<DrvGraph>, root_outputs: Vec<String>) -> Result<()> {
		let wanted = graph.wanted_outputs(&root_outputs);

		self.substitute_prepass(&graph, &wanted).await?;
		self.build_topo(&graph, wanted).await
	}

	async fn substitute_prepass(
		&self,
		graph: &DrvGraph,
		wanted: &HashMap<String, Vec<String>>,
	) -> Result<()> {
		let paths = collect_substitute_paths(graph, wanted);
		if paths.is_empty() {
			return Ok(());
		}
		let _ = self
			.events
			.send(BuildEvent::SubstitutePrepassStarted { paths: paths.len() });
		debug!("substitute pre-pass: {} paths", paths.len());

		let satisfied = tokio::task::spawn_blocking(move || crate::substitute_paths(&paths))
			.await
			.expect("substitute pre-pass task should not panic")?;

		let _ = self.events.send(BuildEvent::SubstitutePrepassFinished {
			satisfied: satisfied.len(),
		});
		Ok(())
	}

	async fn build_topo(
		&self,
		graph: &Arc<DrvGraph>,
		wanted: HashMap<String, Vec<String>>,
	) -> Result<()> {
		let mut indeg: HashMap<String, usize> = graph
			.nodes
			.iter()
			.map(|(k, n)| (k.clone(), n.input_drvs.len()))
			.collect();
		let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
		for (path, node) in &graph.nodes {
			for dep in node.input_drvs.keys() {
				dependents
					.entry(dep.clone())
					.or_default()
					.push(path.clone());
			}
		}

		let sem = Arc::new(Semaphore::new(self.parallelism));
		let mut ready: Vec<String> = indeg
			.iter()
			.filter(|(_, d)| **d == 0)
			.map(|(k, _)| k.clone())
			.collect();
		let mut in_flight = FuturesUnordered::new();
		let mut failed: HashMap<String, String> = HashMap::new();
		// Tainted = transitively depends on a failed drv
		let mut tainted: HashMap<String, String> = HashMap::new();

		loop {
			let batch: Vec<String> = std::mem::take(&mut ready);
			for path in batch {
				if let Some(failed_dep) = tainted.get(&path) {
					let name = graph
						.nodes
						.get(&path)
						.map(|n| n.name.clone())
						.unwrap_or_default();
					let _ = self.events.send(BuildEvent::DrvCancelled {
						drv_path: path.clone(),
						name,
						failed_dep: failed_dep.clone(),
					});
					propagate_done(&dependents, &mut indeg, &mut ready, &path);
					continue;
				}

				let sem = sem.clone();
				let events = self.events.clone();
				let graph = graph.clone();
				let wanted_here = wanted.get(&path).cloned().unwrap_or_default();
				in_flight.push(tokio::spawn(async move {
					let _permit = sem.acquire_owned().await.expect("semaphore not closed");
					let node = graph
						.nodes
						.get(&path)
						.expect("ready node must be in graph")
						.clone();
					let name = node.name.clone();

					let all_valid = !wanted_here.is_empty()
						&& wanted_here.iter().all(|o| {
							node.outputs
								.get(o)
								.map(|p| crate::is_valid_path(p).unwrap_or(false))
								.unwrap_or(false)
						});
					if all_valid {
						let _ = events.send(BuildEvent::DrvSkipped {
							drv_path: path.clone(),
							name: name.clone(),
						});
						return (path, name, Ok::<(), anyhow::Error>(()));
					}

					let _ = events.send(BuildEvent::DrvStarted {
						drv_path: path.clone(),
						name: name.clone(),
						wanted: wanted_here.clone(),
					});

					let path_for_build = path.clone();
					let res = tokio::task::spawn_blocking(move || {
						crate::build_drv_outputs(&path_for_build, &wanted_here)
					})
					.await
					.expect("build task should not panic");

					match res {
						Ok(_) => {
							let _ = events.send(BuildEvent::DrvFinished {
								drv_path: path.clone(),
								name: name.clone(),
							});
							(path, name, Ok(()))
						}
						Err(e) => {
							let msg = format!("{e:#}");
							let _ = events.send(BuildEvent::DrvFailed {
								drv_path: path.clone(),
								name: name.clone(),
								error: msg,
							});
							(path, name, Err(e))
						}
					}
				}));
			}

			let Some(joined) = in_flight.next().await else {
				break;
			};
			let (finished, _name, res) = match joined {
				Ok(t) => t,
				Err(e) => bail!("scheduler task panicked: {e}"),
			};
			match res {
				Ok(()) => {
					propagate_done(&dependents, &mut indeg, &mut ready, &finished);
				}
				Err(e) => {
					failed.insert(finished.clone(), format!("{e:#}"));
					mark_tainted(&dependents, &finished, &mut tainted);
					propagate_done(&dependents, &mut indeg, &mut ready, &finished);
				}
			}
		}

		let stuck: Vec<_> = indeg
			.iter()
			.filter(|(_, d)| **d != 0)
			.map(|(k, _)| k.as_str())
			.collect();
		if !stuck.is_empty() {
			warn!(
				"scheduler finished with {} nodes still pending (loop?)",
				stuck.len()
			);
		}

		if failed.is_empty() {
			info!("scheduler completed");
			Ok(())
		} else {
			let mut report = format!("{} drv(s) failed to build:", failed.len());
			let mut sorted: Vec<_> = failed.iter().collect();
			sorted.sort_by(|a, b| a.0.cmp(b.0));
			for (path, err) in sorted {
				let name = graph
					.nodes
					.get(path)
					.map(|n| n.name.as_str())
					.unwrap_or("?");
				let chain = path_to_root(graph, path);
				report.push_str(&format!(
					"\n\n  {name} ({path}):\n    {err}\n    needed by: {}",
					chain.join(" => "),
				));
			}
			Err(anyhow::anyhow!(report))
		}
	}
}

fn propagate_done(
	dependents: &HashMap<String, Vec<String>>,
	indeg: &mut HashMap<String, usize>,
	ready: &mut Vec<String>,
	finished: &str,
) {
	if let Some(deps) = dependents.get(finished) {
		for d in deps {
			let entry = indeg.get_mut(d).expect("dependent must have indeg");
			*entry = entry.saturating_sub(1);
			if *entry == 0 {
				ready.push(d.clone());
			}
		}
	}
}

fn mark_tainted(
	dependents: &HashMap<String, Vec<String>>,
	failed: &str,
	tainted: &mut HashMap<String, String>,
) {
	let mut queue: Vec<String> = dependents.get(failed).cloned().unwrap_or_default();
	while let Some(node) = queue.pop() {
		if tainted
			.entry(node.clone())
			.or_insert_with(|| failed.to_owned())
			== failed
		{
			if let Some(deps) = dependents.get(&node) {
				for d in deps {
					if !tainted.contains_key(d) {
						queue.push(d.clone());
					}
				}
			}
		}
	}
}

fn path_to_root(graph: &DrvGraph, from: &str) -> Vec<String> {
	let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
	for (path, node) in &graph.nodes {
		for dep in node.input_drvs.keys() {
			dependents
				.entry(dep.as_str())
				.or_default()
				.push(path.as_str());
		}
	}

	let mut chain: Vec<String> = vec![node_name(graph, from)];
	let mut cur = from;
	let mut seen: HashSet<&str> = HashSet::new();
	seen.insert(cur);
	while cur != graph.root.as_str() {
		let Some(next) = dependents.get(cur).and_then(|v| v.first().copied()) else {
			break;
		};
		if !seen.insert(next) {
			break;
		}
		chain.push(node_name(graph, next));
		cur = next;
	}
	chain
}

fn node_name(graph: &DrvGraph, path: &str) -> String {
	graph
		.nodes
		.get(path)
		.map(|n| n.name.clone())
		.unwrap_or_else(|| path.to_owned())
}

fn collect_substitute_paths(
	graph: &DrvGraph,
	wanted: &HashMap<String, Vec<String>>,
) -> Vec<String> {
	let mut paths: HashSet<String> = HashSet::new();
	for node in graph.nodes.values() {
		for src in &node.input_srcs {
			paths.insert(src.clone());
		}
	}
	for (path, outs) in wanted {
		let Some(node) = graph.nodes.get(path) else {
			continue;
		};
		for o in outs {
			if let Some(p) = node.outputs.get(o) {
				paths.insert(p.clone());
			}
		}
	}
	let mut v: Vec<_> = paths.into_iter().collect();
	v.sort();
	v
}

// TODO: Parallelism as a metric works poorly with multiple machines, but I haven't thought about bringing
// hercy here yet. In case of remote machines - they will handle parallelism on their own, and this one
// will work as a hard cap.
pub fn build_graph_sync(graph: Arc<DrvGraph>, root_outputs: Vec<String>) -> Result<()> {
	let parallelism = std::thread::available_parallelism()
		.map(|p| p.get())
		.unwrap_or(4);
	let scheduler = Scheduler::new(parallelism);
	crate::await_in_nix(async move { scheduler.run(graph, root_outputs).await })
		.context("scheduler run")
}
