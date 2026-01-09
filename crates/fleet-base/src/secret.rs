use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::fleetdata::{Expectations, FleetSecretData, FleetSecretDistribution, GeneratorPart};

#[derive(thiserror::Error, Debug)]
pub enum RegenerationReason {
	#[error("owners added: {0:?}")]
	OwnersAdded(BTreeSet<String>),
	#[error("owners added: {0:?}")]
	OwnersRemoved(BTreeSet<String>),
	#[error("unexpected generation data, expected: {expected:?}, found: {found:?}")]
	GenerationData {
		expected: serde_json::Value,
		found: serde_json::Value,
	},
	#[error("unexpected part list, expected: {expected:?}, found: {found:?}")]
	PartList {
		expected: BTreeSet<String>,
		found: BTreeSet<String>,
	},
	#[error("part {0} is expected to be encrypted")]
	ExpectedPrivate(String),
	#[error("part {0} is not expected to be encrypted")]
	ExpectedPublic(String),
	#[error("secret is expired at {0}")]
	Expired(DateTime<Utc>),

	#[error("secret is not generated for this host")]
	Missing,
}

pub fn secret_needs_regeneration(
	secret: &FleetSecretDistribution,
	expectations: &Expectations,
) -> Option<RegenerationReason> {
	let added: BTreeSet<String> = expectations
		.owners
		.difference(&secret.owners)
		.cloned()
		.collect();
	if !added.is_empty() {
		return Some(RegenerationReason::OwnersAdded(added));
	}

	let removed: BTreeSet<String> = secret
		.owners
		.difference(&expectations.owners)
		.cloned()
		.collect();
	if !removed.is_empty() {
		return Some(RegenerationReason::OwnersRemoved(removed));
	}

	if secret.secret.generation_data != expectations.generation_data {
		return Some(RegenerationReason::GenerationData {
			expected: expectations.generation_data.clone(),
			found: secret.secret.generation_data.clone(),
		});
	}

	let expected: BTreeSet<String> = expectations.parts.keys().cloned().collect();
	let found: BTreeSet<String> = secret.secret.parts.keys().cloned().collect();

	if found != expected {
		return Some(RegenerationReason::PartList { expected, found });
	}

	for (name, value) in secret.secret.parts.iter() {
		let expectation = expectations
			.parts
			.get(name)
			.expect("found == expected checked");
		if value.raw.encrypted {
			if !expectation.encrypted {
				return Some(RegenerationReason::ExpectedPrivate(name.clone()));
			}
		} else if expectation.encrypted {
			return Some(RegenerationReason::ExpectedPublic(name.clone()));
		}
	}

	if let Some(expiration) = secret.secret.expires_at {
		// TODO: Leeway?
		if expiration < Utc::now() {
			return Some(RegenerationReason::Expired(expiration));
		}
	}

	None
}
