use std::future::pending;

use anyhow::Result;
use bifrostlink::{Rpc, Rtt};
use bifrostlink_ports::stdio::from_stdio;
use tokio::runtime::Builder;

pub mod host;

pub use bifrostlink;
pub use remowt_link_shared::{self, Address, BifConfig};

pub fn plugin_index() -> Result<u16> {
	let arg = std::env::args()
		.nth(1)
		.ok_or_else(|| anyhow::anyhow!("missing plugin index argument"))?;
	arg.parse()
		.map_err(|e| anyhow::anyhow!("invalid plugin index {arg:?}: {e}"))
}

pub fn host_address() -> Result<Address> {
	let arg = std::env::args()
		.nth(2)
		.ok_or_else(|| anyhow::anyhow!("missing host address argument"))?;
	serde_json::from_str(&arg).map_err(|e| anyhow::anyhow!("invalid host address {arg:?}: {e}"))
}

pub fn run<F>(register: F) -> Result<()>
where
	F: FnOnce(&mut Rpc<BifConfig>),
{
	tracing_subscriber::fmt()
		.with_writer(std::io::stderr)
		.init();

	let index = plugin_index()?;
	let host = host_address()?;
	let runtime = Builder::new_current_thread().enable_all().build()?;
	runtime.block_on(async move {
		let mut rpc = Rpc::<BifConfig>::new(Address::Plugin(index));
		rpc.add_direct(host, from_stdio(), Rtt(0));
		register(&mut rpc);
		let _rpc = rpc;
		pending::<Result<()>>().await
	})
}
