use std::{
	collections::{
		BTreeMap, BTreeSet,
		btree_map::{self, Entry},
	},
	io::{self, Cursor},
	ops::Deref,
};

use age::Recipient;
use chrono::{DateTime, Utc};
use fleet_shared::SecretData;
use rand::{
	distr::{Alphanumeric, SampleString as _},
	rng,
};
use serde::{
	Deserialize, Serialize,
	de::{self, Error},
};
use serde_json::Value;
use tracing::info;

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

	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub manager_keys: Vec<ManagerKey>,

	#[serde(default)]
	pub hosts: BTreeMap<String, HostData>,

	#[serde(default, alias = "shared_secrets")]
	pub secrets: FleetSecrets,

	// extra_name => anything
	#[serde(default)]
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub extra: BTreeMap<String, Value>,

	#[serde(default)]
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	host_secrets: BTreeMap<String, BTreeMap<String, FleetSecretDistribution>>,
}
impl FleetData {
	pub fn from_str(s: &str) -> anyhow::Result<Self> {
		let mut data: Self = nixlike::parse_str(s)?;
		if !data.host_secrets.is_empty() {
			info!("migrating host secrets into shared secrets structure");
			data.secrets
				.merge_from_hosts(std::mem::take(&mut data.host_secrets));
		}
		Ok(data)
	}
}

/// Returns None if recipients.is_empty()
pub fn encrypt_secret_data<'r>(
	recipients: impl IntoIterator<Item = &'r Box<dyn Recipient>>,
	data: Vec<u8>,
) -> Option<SecretData> {
	let mut encrypted = vec![];
	let mut encryptor = age::Encryptor::with_recipients(recipients.into_iter().map(|v| &**v))
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FleetSecretPart {
	pub raw: SecretData,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[must_use]
pub struct FleetSecretData {
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

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[must_use]
pub struct FleetSecretDistribution {
	#[serde(default)]
	pub owners: BTreeSet<String>,
	#[serde(flatten)]
	pub secret: FleetSecretData,

	#[serde(default, skip_serializing, alias = "managed")]
	pub _deprecated_managed: bool,
}

#[derive(Clone)]
#[must_use]
pub struct FleetSecretDistributions(Vec<FleetSecretDistribution>);

impl Deref for FleetSecretDistributions {
	type Target = [FleetSecretDistribution];

	fn deref(&self) -> &Self::Target {
		self.0.as_slice()
	}
}

impl FleetSecretDistributions {
	pub fn owners(&self) -> impl Iterator<Item = &String> {
		self.0.iter().flat_map(|v| v.owners.iter())
	}
	#[allow(
		clippy::len_without_is_empty,
		reason = "should not be empty for a long time"
	)]
	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn get(&self, owner: &str) -> Option<&FleetSecretDistribution> {
		self.0.iter().find(|d| d.owners.contains(owner))
	}
	fn entry(&mut self, owner: String) -> DistEntry<'_> {
		let Some(idx) = self.0.iter().position(|d| d.owners.contains(&owner)) else {
			return DistEntry::Vacant(VacantDistEntry {
				distributions: self,
				owner,
			});
		};
		DistEntry::Occupied(OccupiedDistEntry {
			distributions: self,
			idx,
			owner,
		})
	}
	fn extend(&mut self, dist: FleetSecretDistribution) {
		for owner in &dist.owners {
			self.entry(owner.to_owned()).remove();
		}
		self.0.push(dist);
	}
	pub fn contains(&self, owner: &str) -> bool {
		self.0.iter().any(|d| d.owners.contains(owner))
	}
}

struct OccupiedDistEntry<'d> {
	distributions: &'d mut FleetSecretDistributions,
	idx: usize,
	owner: String,
}
impl<'d> OccupiedDistEntry<'d> {
	fn remove(self) -> VacantDistEntry<'d> {
		let dist = &mut self.distributions.0[self.idx];
		assert!(
			dist.owners.remove(&self.owner),
			"entry exists, as we have its reference"
		);
		if dist.owners.is_empty() {
			self.distributions.0.remove(self.idx);
		}
		VacantDistEntry {
			distributions: self.distributions,
			owner: self.owner,
		}
	}
	fn set(self, secret: FleetSecretData) -> Self {
		self.remove().set(secret)
	}
}
struct VacantDistEntry<'d> {
	distributions: &'d mut FleetSecretDistributions,
	owner: String,
}
impl<'d> VacantDistEntry<'d> {
	fn set(self, secret: FleetSecretData) -> OccupiedDistEntry<'d> {
		let Self {
			distributions,
			owner,
		} = self;
		let idx = distributions.0.len();
		distributions.0.push(FleetSecretDistribution {
			owners: BTreeSet::from_iter([owner.clone()]),
			secret,

			_deprecated_managed: true,
		});
		OccupiedDistEntry {
			distributions,
			owner,
			idx,
		}
	}
}

enum DistEntry<'d> {
	Vacant(VacantDistEntry<'d>),
	Occupied(OccupiedDistEntry<'d>),
}
impl DistEntry<'_> {
	fn remove(self) -> Self {
		match self {
			DistEntry::Vacant(_) => self,
			DistEntry::Occupied(o) => Self::Vacant(o.remove()),
		}
	}
	fn set(self, secret: FleetSecretData) -> Self {
		Self::Occupied(match self {
			DistEntry::Vacant(e) => e.set(secret),
			DistEntry::Occupied(e) => e.set(secret),
		})
	}
}

impl Serialize for FleetSecretDistributions {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut found_hosts = BTreeSet::new();
		for ele in self.0.iter() {
			if ele.owners.is_empty() {
				panic!("consistency: secret distribution has no defined owners");
			}
			for ele in ele.owners.iter() {
				if !found_hosts.insert(ele) {
					panic!(
						"consistency: secret distribution contains duplicate entry for the same host",
					);
				}
			}
		}
		match self.0.len() {
			0 => panic!("consistency: empty distributions"),
			1 => self.0[0].serialize(serializer),
			_ => self.0.serialize(serializer),
		}
	}
}
impl<'de> Deserialize<'de> for FleetSecretDistributions {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum Distributions {
			One(FleetSecretDistribution),
			Many(Vec<FleetSecretDistribution>),
		}
		let d = Distributions::deserialize(deserializer)?;
		let ds = match d {
			Distributions::One(d) => vec![d],
			Distributions::Many(ds) => ds,
		};
		if ds.is_empty() {
			return Err(de::Error::custom("consistency: empty distributions"));
		}
		let mut found_hosts = BTreeSet::new();
		for ele in ds.iter() {
			if ele.owners.is_empty() {
				return Err(de::Error::custom(
					"consistency: secret distribution has no defined owners",
				));
			}
			for ele in ele.owners.iter() {
				if !found_hosts.insert(ele) {
					return Err(de::Error::custom(
						"consistency: secret distribution contains duplicate entry for the same host",
					));
				}
			}
		}
		Ok(Self(ds))
	}
}

#[derive(Serialize, Deserialize, Default)]
pub struct FleetSecrets(BTreeMap<String, FleetSecretDistributions>);

impl FleetSecrets {
	pub fn keys(&self) -> btree_map::Keys<String, FleetSecretDistributions> {
		self.0.keys()
	}

	pub fn keys_for_owner(&self, owner: &str) -> impl Iterator<Item = &String> {
		self.0
			.iter()
			.filter(|(_, d)| d.contains(owner))
			.map(|(n, _)| n)
	}

	pub fn drop_owner_no_reencrypt(&mut self, secret: &str, owner: &str) -> bool {
		let Entry::Occupied(mut dists) = self.0.entry(secret.to_owned()) else {
			return false;
		};
		let DistEntry::Occupied(dist) = dists.get_mut().entry(owner.to_owned()) else {
			return false;
		};

		dist.remove();

		if dists.get().0.is_empty() {
			dists.remove();
		};

		true
	}
	pub fn set_single_data(&mut self, secret: String, owner: String, data: FleetSecretData) {
		let e = self
			.0
			.entry(secret.to_owned())
			.or_insert_with(|| FleetSecretDistributions(Default::default()));
		e.entry(owner.to_owned()).set(data);
	}
	pub fn set_data(&mut self, secret: String, data: FleetSecretDistribution) {
		match self.0.entry(secret) {
			Entry::Vacant(e) => {
				e.insert(FleetSecretDistributions(vec![data]));
			}
			Entry::Occupied(mut e) => {
				let dists = e.get_mut();
				dists.extend(data)
			}
		}
	}
	pub fn get_single(&self, secret: &str, owner: &str) -> Option<&FleetSecretDistribution> {
		let secret = self.0.get(secret)?;
		secret.get(owner)
	}
	pub fn get(&self, secret: &str) -> Option<&FleetSecretDistributions> {
		self.0.get(secret)
	}

	pub fn contains_for_owner(&self, secret: &str, owner: &str) -> bool {
		let Some(secret) = self.0.get(secret) else {
			return false;
		};
		secret.contains(owner)
	}
	pub fn contains(&self, secret: &str) -> bool {
		self.0.contains_key(secret)
	}
	pub fn remove(&mut self, secret: &str) {
		self.0.remove(secret);
	}

	fn merge_from_hosts(
		&mut self,
		host_secrets: BTreeMap<String, BTreeMap<String, FleetSecretDistribution>>,
	) {
		for (host, host_secrets) in host_secrets {
			for (secret_name, mut secret_data) in host_secrets {
				secret_data.owners.insert(host.clone());
				self.set_data(secret_name, secret_data);
			}
		}
	}
}

#[derive(Debug)]
pub struct Expectations {
	pub owners: BTreeSet<String>,
	pub generation_data: serde_json::Value,
	pub parts: BTreeMap<String, GeneratorPart>,
}
#[derive(Deserialize, Debug, Clone)]
pub struct GeneratorPart {
	pub encrypted: bool,
}
