use std::time::Instant;

use anyhow::bail;
use tracing::{debug, warn};

use crate::{Value, nix_go_json};

#[tracing::instrument(level = "info", skip(val))]
pub async fn assert_warn(action: &str, val: &Value) -> anyhow::Result<()> {
	let before_errors = Instant::now();
	let errors: Vec<String> = nix_go_json!(val.errors);
	debug!("errors evaluation took {:?}", before_errors.elapsed());
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
