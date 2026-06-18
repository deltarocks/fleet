use std::future::Future;

use bifrostlink::declarative::endpoints;
use bifrostlink::{Config, Rpc};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("editor failed: {0}")]
	Failed(String),
}

pub trait EditorBackend: Send + Sync {
	fn open_editor(&self, socket_path: String) -> impl Future<Output = Result<(), Error>> + Send;
	fn expose_tcp(&self, addr: String) -> impl Future<Output = Result<u16, Error>> + Send;
	fn expose_udp(&self, addr: String) -> impl Future<Output = Result<u16, Error>> + Send;
}

pub struct EditorEndpoints<E>(pub E);

#[endpoints(ns = 8)]
impl<E: EditorBackend + 'static> EditorEndpoints<E> {
	#[endpoints(id = 1)]
	async fn open_editor(&self, socket_path: String) -> Result<(), Error> {
		self.0.open_editor(socket_path).await
	}

	#[endpoints(id = 2)]
	async fn expose_tcp(&self, addr: String) -> Result<u16, Error> {
		self.0.expose_tcp(addr).await
	}

	#[endpoints(id = 3)]
	async fn expose_udp(&self, addr: String) -> Result<u16, Error> {
		self.0.expose_udp(addr).await
	}
}

pub fn serve_editor<E, C>(rpc: &mut Rpc<C>, editor: E)
where
	E: EditorBackend + Send + Sync + 'static,
	C: Config,
{
	EditorEndpoints(editor).register_endpoints(rpc);
}
