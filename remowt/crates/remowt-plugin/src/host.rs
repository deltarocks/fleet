use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::Mutex;

use bifrostlink::{Rpc, Rtt, WeakRpc};
use tokio::process::{Child, Command};

use remowt_link_shared::plugin::{Error, PluginEndpoints, PluginHost};
use remowt_link_shared::port::child_port;
use remowt_link_shared::{Address, BifConfig};
use tokio::select;

pub fn serve(rpc: &mut Rpc<BifConfig>) {
	let host = Host {
		me: rpc.me(),
		rpc: rpc.clone().downgrade(),
		children: Mutex::new(Vec::new()),
	};
	PluginEndpoints(host).register_endpoints(rpc);
}

struct Host {
	me: Address,
	rpc: WeakRpc<BifConfig>,
	children: Mutex<Vec<Child>>,
}

impl Host {
	async fn spawn(&self, id: u16, path: impl AsRef<OsStr>) -> Result<(), Error> {
		let rpc = self.rpc.clone().upgrade().ok_or(Error::Gone)?;

		let mut child = Command::new(path)
			.arg(id.to_string())
			.arg(serde_json::to_string(&self.me).expect("address serializes"))
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|e| Error::Spawn(e.to_string()))?;
		let stdin = child.stdin.take().expect("stdin piped");
		let stdout = child.stdout.take().expect("stdout piped");

		let addr = Address::Plugin(id);
		rpc.add_direct(addr.clone(), child_port(stdout, stdin), Rtt(0));

		select! {
			e = rpc.wait_for_connection_to(addr) => {
				if e.is_err() {
					return Err(Error::ConnectionWaitError)
				}
			},
			_ = child.wait() => {
				return Err(Error::ConnectionWaitError)
			}
		};
		self.children.lock().expect("not poisoned").push(child);

		Ok(())
	}
}

impl PluginHost for Host {
	async fn load_plugin(&self, id: u16, name: String) -> Result<(), Error> {
		// TODO: Right now loads plugin next to the binary...
		// But with our CA addressed schema, the plugins should be located in content-addressed subdir...
		// Maybe it should just be scrapped in favor of load_plugin_path.
		if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
			return Err(Error::BadName);
		}
		let exe = std::env::current_exe().map_err(|e| Error::Spawn(e.to_string()))?;
		let dir = exe
			.parent()
			.ok_or_else(|| Error::Spawn("primary agent has no parent directory".to_owned()))?;
		self.spawn(id, dir.join(&name)).await
	}

	async fn load_plugin_path(&self, id: u16, path: String) -> Result<(), Error> {
		if path.is_empty() || path.contains('\0') {
			return Err(Error::BadName);
		}
		self.spawn(id, path).await
	}
}
