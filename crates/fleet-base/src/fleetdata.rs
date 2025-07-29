use std::{
	collections::BTreeMap,
	io::{self, Cursor},
};

use age::Recipient;
use chrono::{DateTime, Utc};
use fleet_shared::SecretData;
use rand::{
	distr::{Alphanumeric, SampleString as _},
	rng,
};
use serde::{Deserialize, Serialize, de::Error};
use serde_json::Value;

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostData {
	#[serde(default)]
	#[serde(skip_serializing_if = "String::is_empty")]
	pub encryption_key: String,
}

const VERSION: &str = "0.1.0";
pub struct FleetDataVersion;
impl Serialize for FleetDataVersion {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		VERSION.serialize(serializer)
	}
}
impl<'de> Deserialize<'de> for FleetDataVersion {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let version = String::deserialize(deserializer)?;
		if version != VERSION {
			return Err(D::Error::custom(format!(
				"fleet.nix data version mismatch, expected {VERSION}, got {version}.\nFollow the docs for migration instruction"
			)));
		}
		Ok(Self)
	}
}

fn generate_gc_prefix() -> String {
	let id = Alphanumeric.sample_string(&mut rng(), 8);
	format!("fleet-gc-{id}")
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerKey {
	pub name: String,
	pub key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetData {
	pub version: FleetDataVersion,
	#[serde(default = "generate_gc_prefix")]
	pub gc_root_prefix: String,

	#[serde(default)]
	pub manager_keys: Vec<ManagerKey>,

	#[serde(default)]
	pub hosts: BTreeMap<String, HostData>,
	#[serde(default)]
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub shared_secrets: BTreeMap<String, FleetSharedSecret>,
	#[serde(default)]
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub host_secrets: BTreeMap<String, BTreeMap<String, FleetSecret>>,

	// extra_name => anything
	#[serde(default)]
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[must_use]
pub struct FleetSharedSecret {
	pub owners: Vec<String>,
	#[serde(flatten)]
	pub secret: FleetSecret,
}

/// Returns None if recipients.is_empty()
pub fn encrypt_secret_data<'a>(
	recipients: impl IntoIterator<Item = &'a dyn Recipient>,
	data: Vec<u8>,
) -> Option<SecretData> {
	let mut encrypted = vec![];
	let mut encryptor = age::Encryptor::with_recipients(recipients.into_iter())
		.ok()?
		.wrap_output(&mut encrypted)
		.expect("in memory write");
	io::copy(&mut Cursor::new(data), &mut encryptor).expect("in memory copy");
	encryptor.finish().expect("in memory flush");
	Some(SecretData {
		data: encrypted,
		encrypted: true,
	})
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FleetSecretPart {
	pub raw: SecretData,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[must_use]
pub struct FleetSecret {
	#[serde(default = "Utc::now")]
	pub created_at: DateTime<Utc>,
	#[serde(default)]
	#[serde(skip_serializing_if = "Option::is_none", alias = "expire_at")]
	pub expires_at: Option<DateTime<Utc>>,

	#[serde(flatten)]
	pub parts: BTreeMap<String, FleetSecretPart>,

	#[serde(default)]
	#[serde(skip_serializing_if = "Value::is_null")]
	pub generation_data: Value,
}
