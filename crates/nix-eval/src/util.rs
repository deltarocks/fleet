use std::time::Instant;

use anyhow::bail;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{Value, nix_go_json};

#[derive(Deserialize, Debug)]
struct Assertion {
	assertion: bool,
	message: String,
}

#[tracing::instrument(level = "info", skip(val))]
pub async fn assert_warn(action: &str, val: &Value) -> anyhow::Result<()> {
	let before_errors = Instant::now();
	let errors: Vec<String> = nix_go_json!(val.errors);
	// let assertions: Vec<Assertion> = nix_go_json!(val.assertions);
	debug!("errors evaluation took {:?} {errors:?} ", before_errors.elapsed());
	if !errors.is_empty() {
		bail!(
			"failed with error{}{}",
			if errors.len() != 1 { "s:\n- " } else { ": " },
			errors.join("\n- "),
		);
	}

	let before_errors = Instant::now();
	let warnings: Vec<String> = nix_go_json!(val.warnings);
	debug!("warnings evaluation took {:?}", before_errors.elapsed());
	if !warnings.is_empty() {
		warn!(
			"completed with warning{}{}",
			if warnings.len() != 1 { "s:\n- " } else { ": " },
			warnings.join("\n- "),
		);
	}
	Ok(())
}
