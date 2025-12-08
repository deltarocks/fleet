use fleet_base::primops::init_primops;

#[unsafe(no_mangle)]
fn nix_plugin_entry() {
	init_primops();
}
