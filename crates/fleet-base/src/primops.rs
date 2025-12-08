use nix_eval::NativeFn;

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

struct FsSecretsBackend {

}

pub fn init_primops() {
	NativeFn::new(
		c"fleet_ensure_secret",
		c"Ensure secret existence for a host, regenerating it in case of some mismatch",
		[
			c"host",
			c"secret",
			c"expected_parts",
			c"expected_encrypted_parts",
			c"generator",
		],
		|[
			host,
			secret,
			expected_parts,
			expected_encrypted_parts,
			generator,
		]| { 

			todo!()
		},
	)
	.register();
}
