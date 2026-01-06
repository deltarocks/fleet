use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nix_eval::{NativeFn, Value};
use tracing::info;

use crate::fleetdata::{FleetData, FleetSecrets};

#[derive(thiserror::Error, Debug)]
enum Error {}

struct Parts {
	encrypted: Vec<String>,
	public: Vec<String>,
}

trait SecretsBackend {
	fn has_shared(&self, name: &str);
	fn has_host(&self, host: &str, name: &str);
	fn shared_parts(&self, name: &str) -> Parts;
	fn host_parts(&self, host: &str, name: &str) -> Parts;
}

struct FsSecretsBackend {}

pub fn init_primops(secrets: Arc<Mutex<FleetData>>) {
	info!("initializing primops");
	NativeFn::new(
		c"__fleetEnsureHostSecret",
		c"Ensure secret existence for a host, regenerating it in case of some mismatch",
		[c"host", c"secret", c"generator"],
		|[host, secret, generator]| {
			todo!("ensure secret");
			Ok(Value::new_attrs(HashMap::from_iter([(
				"raw",
				Value::new_str("rawData"),
			)])))
		},
	)
	.register();
}
