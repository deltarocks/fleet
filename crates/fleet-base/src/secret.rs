use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::fleetdata::FleetSecretData;

#[derive(Debug)]
pub struct Expectations {
	pub owners: BTreeSet<String>,
	pub generation_data: serde_json::Value,
	pub public_parts: BTreeSet<String>,
	pub private_parts: BTreeSet<String>,
}

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
}

pub fn secret_needs_regeneration(
	secret: &FleetSecretData,
	owners: &BTreeSet<String>,
	expectations: &Expectations,
) -> Option<RegenerationReason> {
	if !owners.is_empty() {
		let added: BTreeSet<String> = expectations.owners.difference(owners).cloned().collect();
		if !added.is_empty() {
			return Some(RegenerationReason::OwnersAdded(added));
		}

		let removed: BTreeSet<String> = owners.difference(&expectations.owners).cloned().collect();
		if !removed.is_empty() {
			return Some(RegenerationReason::OwnersRemoved(removed));
		}
	}

	if secret.generation_data != expectations.generation_data {
		return Some(RegenerationReason::GenerationData {
			expected: expectations.generation_data.clone(),
			found: secret.generation_data.clone(),
		});
	}

	if !expectations.public_parts.is_empty() || !expectations.private_parts.is_empty() {
		let expected: BTreeSet<String> = expectations
			.public_parts
			.union(&expectations.private_parts)
			.cloned()
			.collect();
		let found: BTreeSet<String> = secret.parts.keys().cloned().collect();

		if found != expected {
			return Some(RegenerationReason::PartList { expected, found });
		}

		for (name, value) in secret.parts.iter() {
			if value.raw.encrypted {
				if !expectations.private_parts.contains(name) {
					return Some(RegenerationReason::ExpectedPrivate(name.clone()));
				}
			} else if !expectations.public_parts.contains(name) {
				return Some(RegenerationReason::ExpectedPublic(name.clone()));
			}
		}
	}

	if let Some(expiration) = secret.expires_at {
		// TODO: Leeway?
		if expiration < Utc::now() {
			return Some(RegenerationReason::Expired(expiration));
		}
	}

	None
}
