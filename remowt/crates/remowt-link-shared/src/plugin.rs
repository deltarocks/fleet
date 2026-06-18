use std::future::Future;

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("plugin name must be a bare file name")]
	BadName,
	#[error("spawning plugin failed: {0}")]
	Spawn(String),
	#[error("failed to wait for connection, plugin process has died?")]
	ConnectionWaitError,
	#[error("agent is shutting down")]
	Gone,
}

pub trait PluginHost: Send + Sync {
	fn load_plugin(&self, id: u16, name: String) -> impl Future<Output = Result<(), Error>> + Send;

	fn load_plugin_path(
		&self,
		id: u16,
		path: String,
	) -> impl Future<Output = Result<(), Error>> + Send;
}

pub struct PluginEndpoints<H>(pub H);

#[endpoints(ns = 9)]
impl<H: PluginHost + 'static> PluginEndpoints<H> {
	#[endpoints(id = 1)]
	async fn load_plugin(&self, id: u16, name: String) -> Result<(), Error> {
		self.0.load_plugin(id, name).await
	}
	#[endpoints(id = 2)]
	async fn load_plugin_path(&self, id: u16, path: String) -> Result<(), Error> {
		self.0.load_plugin_path(id, path).await
	}
}
