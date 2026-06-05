use fleet_base::primops::init_primops;

/// SAFETY: expected plugin dynamic library entry point
#[unsafe(no_mangle)]
fn nix_plugin_entry() {
	init_primops();
}
