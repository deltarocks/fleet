use remowt_fleet::{Nix, init_libraries};

fn main() -> anyhow::Result<()> {
	remowt_plugin::run(|rpc| {
		init_libraries();
		Nix.register_endpoints(rpc);
	})
}
