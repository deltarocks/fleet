use remowt_fleet::{init_libraries, Nix};

fn main() -> anyhow::Result<()> {
	remowt_plugin::run(|rpc| {
		init_libraries();
		Nix.register_endpoints(rpc);
	})
}
