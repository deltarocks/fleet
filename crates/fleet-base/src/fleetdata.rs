use std::{
	cmp::Ordering,
	collections::{
		BTreeMap, BTreeSet,
		btree_map::{self, Entry},
	},
	fmt,
	io::{self, Cursor},
	sync::RwLock,
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
	pub hosts: RwLock<BTreeMap<String, HostData>>,

	#[serde(default, alias = "shared_secrets")]
	pub secrets: RwLock<FleetSecrets>,

	// extra_name => anything
	#[serde(default)]
	pub extra: RwLock<BTreeMap<String, Value>>,

	#[serde(default)]
	#[serde(skip_serializing)]
	host_secrets: BTreeMap<SecretOwner, BTreeMap<String, FleetSecretDistribution>>,
}
impl FleetData {
	pub fn from_str(s: &str) -> anyhow::Result<Self> {
		let mut data: Self = nixlike::parse_str(s)?;
		if !data.host_secrets.is_empty() {
			info!("migrating host secrets into shared secrets structure");
			data.secrets
				.write()
				.expect("no poisoning")
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
	pub created_at: DateTime<Utc>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "expire_at")]
	pub expires_at: Option<DateTime<Utc>>,

	#[serde(flatten)]
	pub parts: BTreeMap<String, FleetSecretPart>,

	#[serde(default, skip_serializing_if = "Value::is_null")]
	pub generation_data: Value,
}

fn is_false(b: &bool) -> bool {
	*b == false
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
#[repr(transparent)]
pub struct SecretOwner(String);

impl fmt::Display for SecretOwner {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "host:{}", self.0)
	}
}

impl SecretOwner {
	pub fn host(s: impl AsRef<str>) -> SecretOwner {
		SecretOwner(s.as_ref().to_owned())
	}
	pub fn as_host(&self) -> Option<&str> {
		Some(&self.0)
	}
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[must_use]
pub struct FleetSecretDistribution {
	#[serde(default)]
	owners: BTreeSet<SecretOwner>,
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	owners_pending_prune: BTreeMap<SecretOwner, String>,

	#[serde(flatten)]
	pub secret: FleetSecretData,

	#[serde(default, skip_serializing_if = "Option::is_none")]
	pending_prune: Option<String>,
	#[serde(default, skip_serializing, alias = "managed")]
	_deprecated_managed: bool,
}

const EMPTY_PENDING_PRUNE: &BTreeMap<SecretOwner, String> = &BTreeMap::new();
impl FleetSecretDistribution {
	pub fn new(owners: BTreeSet<SecretOwner>, secret: FleetSecretData, now: DateTime<Utc>) -> Self {
		assert!(
			!owners.is_empty(),
			"distribution should have at least one owner"
		);
		if let Some(expires_at) = &secret.expires_at {
			assert!(
				*expires_at > now,
				"secret should not be expired on creation"
			);
		}
		Self {
			owners,
			secret,
			owners_pending_prune: BTreeMap::new(),
			pending_prune: None,
			_deprecated_managed: true,
		}
	}

	fn owners_ex(&self, including_pruned: bool) -> impl Iterator<Item = &SecretOwner> {
		let pending_prune = if including_pruned {
			&self.owners_pending_prune
		} else {
			EMPTY_PENDING_PRUNE
		};
		self.owners.iter().chain(pending_prune.keys())
	}
	pub fn owners(&self) -> impl Iterator<Item = &SecretOwner> {
		self.owners_ex(false)
	}
	pub fn owners_pending_prune(&self) -> impl Iterator<Item = &SecretOwner> {
		self.owners_pending_prune.keys()
	}
	pub fn is_pending_prune(&self) -> bool {
		self.pending_prune.is_some()
	}

	pub fn prune(&mut self, reason: String) {
		assert!(
			self.pending_prune.is_none(),
			"it shouldn't be possible to prune the same distribution twice using public api"
		);
		self.pending_prune = Some(reason);
	}
	pub fn prune_owners(&mut self, owners: &BTreeSet<SecretOwner>, reason: String) {
		// if self.owners.iter().all(|o| owners.contains(o)) && self.owners_pending_prune.is_empty() {
		// 	self.prune(format!("all owners were pruned: {reason}"));
		// 	return;
		// }
		for owner in owners {
			if self.owners.remove(owner) {
				self.owners_pending_prune
					.insert(owner.to_owned(), reason.clone());
			}
		}
		// if self.owners.is_empty() {
		// 	self.prune("no owners left".to_owned());
		// }
	}
	pub fn unprune_owner(&mut self, owner: SecretOwner) {
		if self.owners_pending_prune.remove(&owner).is_some() {
			self.owners.insert(owner);
		}
	}
}

#[derive(Clone, Debug, Default)]
#[must_use]
pub struct FleetSecretDistributions {
	stored: Vec<FleetSecretDistribution>,
}

fn compare_dists(
	a: &FleetSecretDistribution,
	b: &FleetSecretDistribution,
	prefer_identities: &BTreeSet<SecretOwner>,
	include_pruned_owners: bool,
) -> Ordering {
	use Ordering::*;
	if prefer_identities.is_empty() {
		let a_has = a
			.owners_ex(include_pruned_owners)
			.any(|o| prefer_identities.contains(o));
		let b_has = b
			.owners_ex(include_pruned_owners)
			.any(|o| prefer_identities.contains(o));
		match (a_has, b_has) {
			(true, false) => return Greater,
			(false, true) => return Less,
			_ => {}
		}
	}
	match (a.secret.expires_at, b.secret.expires_at) {
		(None, Some(_)) => return Greater,
		(Some(_), None) => return Less,
		(Some(a), Some(b)) => {
			// Later is better
			return a.cmp(&b);
		}
		(None, None) => {}
	}

	// Which one is easier to access
	return a.owners.len().cmp(&b.owners.len());
}

impl FleetSecretDistributions {
	/// Drop expired distributions
	fn prune_expired(&mut self, now: DateTime<Utc>) {
		for ele in self.distributions_mut() {
			if let Some(expires_at) = ele.secret.expires_at {
				if expires_at < now {
					ele.prune(format!("expired during check at {now}"));
				}
			}
		}
	}
	/// Perform all pruning relevant to shared secrets
	/// Also see expected_owner_removed
	pub fn prune_shared(
		&mut self,
		expected_owners: &BTreeSet<SecretOwner>,
		unique: bool,
		expected_parts: &BTreeMap<String, GeneratorPart>,
		expected_generation_data: &Value,
		regenerate_on_owner_removed: bool,
		regenerate_on_owner_added: bool,
		prefer_identities: &BTreeSet<SecretOwner>,
		now: DateTime<Utc>,
	) {
		self.prune_expired(now);
		self.prune_generation_data(expected_generation_data, None);
		self.prune_missing_parts(expected_parts, None);

		let current_owners = self.owners().cloned().collect::<BTreeSet<SecretOwner>>();

		let mut to_add = expected_owners.difference(&current_owners);
		if to_add.next().is_some() && unique && regenerate_on_owner_added {
			for dist in self.distributions_mut() {
				dist.prune(format!(
					"owners missing, can't add new distribution, regeneration preferred"
				));
			}
			return;
		}

		for to_remove in current_owners.difference(&expected_owners) {
			self.entry(to_remove.clone()).remove(
				regenerate_on_owner_removed,
				"owner was removed from expected owners list, regenerate_on_owner_removed is set"
					.to_string(),
			);
		}
		if unique {
			self.prune_nonunique(prefer_identities);
		}
	}
	pub fn prune_host(
		&mut self,
		owner: SecretOwner,
		expected_parts: &BTreeMap<String, GeneratorPart>,
		expected_generation_data: &Value,
		now: DateTime<Utc>,
	) {
		self.prune_expired(now);
		self.prune_generation_data(expected_generation_data, Some(&owner));
		// TODO: Owner-based pruning is warranted (e.g host no longer has secret defined)
		self.prune_missing_parts(expected_parts, Some(&owner));
	}
	/// Position of best distributions as in iterator returned by distributions()
	/// None if distributions not found
	fn best_idx(
		&self,
		prefer_identities: &BTreeSet<SecretOwner>,
		include_pruned_owners: bool,
	) -> Option<usize> {
		self.distributions()
			.enumerate()
			.max_by(|(_, a), (_, b)| {
				compare_dists(&a, &b, prefer_identities, include_pruned_owners)
			})
			.map(|(p, _)| p)
	}
	/// Secret wants to be the same on all hosts, leave only one unpruned version of it
	fn prune_nonunique(&mut self, prefer_identities: &BTreeSet<SecretOwner>) {
		if self.distributions().next().is_none() {
			return;
		}
		let best = self.best_idx(prefer_identities, false).expect("not empty");
		for (i, dist) in self.distributions_mut().enumerate() {
			if i != best {
				dist.prune(
					"secret wants to be the same on all hosts, only the best one was left"
						.to_owned(),
				);
			}
		}
	}

	pub fn try_unprune(&mut self, owner: SecretOwner) -> Option<&FleetSecretDistribution> {
		assert!(self.get(&owner).is_none(), "secret is not pruned for host");
		if let Some(dist) = self
			.distributions_mut()
			.find(|v| v.owners_pending_prune.contains_key(&owner))
		{
			dist.unprune_owner(owner);
			Some(dist)
		} else {
			None
		}
	}

	pub fn best_distribution_for_reencryption(
		&mut self,
		prefer_identities: &BTreeSet<SecretOwner>,
	) -> Option<&mut FleetSecretDistribution> {
		let best_idx = self.best_idx(prefer_identities, true)?;
		self.distributions_mut().nth(best_idx)
	}

	fn prune_missing_parts(
		&mut self,
		expected_parts: &BTreeMap<String, GeneratorPart>,
		filter_owner: Option<&SecretOwner>,
	) {
		'dist: for ele in self.distributions_mut() {
			if let Some(filter_owner) = filter_owner {
				if !ele.owners.contains(filter_owner) {
					continue;
				}
				// Note: secret still can have multiple owners even if it is host-owned
				// in this case we expect that all owners using the same generator, so we can prune distribution for all of them
			}
			for (name, part) in expected_parts {
				let Some(stored_part) = ele.secret.parts.get(name) else {
					ele.prune(format!("secret definition added new part: {name}"));
					continue 'dist;
				};
				if part.encrypted != stored_part.raw.encrypted {
					ele.prune(format!(
						"secret definition now requires part to be {}",
						if part.encrypted {
							"encrypted"
						} else {
							"non-encrypted"
						}
					));
					continue 'dist;
				}
			}
		}
	}
	fn prune_generation_data(
		&mut self,
		expected_generation_data: &Value,
		filter_owner: Option<&SecretOwner>,
	) {
		for ele in self.distributions_mut() {
			if let Some(filter_owner) = filter_owner {
				if !ele.owners.contains(filter_owner) {
					continue;
				}
				// Note: secret still can have multiple owners even if it is host-owned
				// in this case we expect that all owners using the same generator, so we can prune distribution for all of them
			}
			if ele.secret.generation_data != *expected_generation_data {
				ele.prune(format!(
					"expected generation data mismatch: {expected_generation_data:?}"
				));
			}
		}
	}

	/// Prune all distributions with no unpruned owners.
	/// For ease of reencryption where possible, it is only called on persistence, when in memory - pruned owners are kept and
	/// can decrypt their secrets.
	fn prune_dead(&mut self) {
		for ele in self.distributions_mut() {
			if ele.owners.is_empty() {
				ele.prune("no owners left".to_owned());
			}
		}
	}

	pub fn all_distributions(&self) -> impl Iterator<Item = &FleetSecretDistribution> {
		self.stored.iter()
	}
	pub fn distributions(&self) -> impl Iterator<Item = &FleetSecretDistribution> {
		self.stored.iter().filter(|v| v.pending_prune.is_none())
	}
	pub fn distributions_mut(&mut self) -> impl Iterator<Item = &mut FleetSecretDistribution> {
		self.stored.iter_mut().filter(|v| v.pending_prune.is_none())
	}
	pub fn owners(&self) -> impl Iterator<Item = &SecretOwner> {
		self.distributions().flat_map(|v| v.owners.iter())
	}
	#[allow(
		clippy::len_without_is_empty,
		reason = "should not be empty for a long time"
	)]
	pub fn len(&self) -> usize {
		self.distributions().count()
	}

	pub fn get(&self, owner: &SecretOwner) -> Option<&FleetSecretDistribution> {
		self.distributions().find(|d| d.owners.contains(owner))
	}
	fn entry(&mut self, owner: SecretOwner) -> DistEntry<'_> {
		let Some((idx, dist)) = self
			.distributions()
			.enumerate()
			.find(|(_, d)| d.owners.contains(&owner))
		else {
			return DistEntry::Vacant(VacantDistEntry {
				distributions: self,
				owners: BTreeSet::from([owner]),
			});
		};
		DistEntry::Occupied(OccupiedDistEntry {
			owners: dist.owners.clone(),
			distributions: self,
			idx,
		})
	}
	pub fn extend(&mut self, dist: FleetSecretDistribution, reason: String) {
		for ele in self.distributions_mut() {
			ele.prune_owners(&dist.owners, reason.clone());
		}
		self.stored.push(dist);
	}
	pub fn contains(&self, owner: &SecretOwner) -> bool {
		self.distributions().any(|d| d.owners.contains(owner))
	}
}

struct OccupiedDistEntry<'d> {
	distributions: &'d mut FleetSecretDistributions,
	idx: usize,
	owners: BTreeSet<SecretOwner>,
}
impl<'d> OccupiedDistEntry<'d> {
	fn remove(self, whole_dist: bool, reason: String) -> VacantDistEntry<'d> {
		let dist = &mut self.distributions.stored[self.idx];
		if whole_dist {
			dist.prune(reason);
		} else {
			dist.prune_owners(&self.owners, reason);
		}
		VacantDistEntry {
			distributions: self.distributions,
			owners: self.owners,
		}
	}
	fn set(self, secret: FleetSecretData, reason: String) -> Self {
		self.remove(false, reason).set(secret)
	}
}
struct VacantDistEntry<'d> {
	distributions: &'d mut FleetSecretDistributions,
	owners: BTreeSet<SecretOwner>,
}
impl<'d> VacantDistEntry<'d> {
	fn set(self, secret: FleetSecretData) -> OccupiedDistEntry<'d> {
		let Self {
			distributions,
			owners,
		} = self;
		let idx = distributions.stored.len();
		distributions.stored.push(FleetSecretDistribution {
			owners: owners.clone(),
			secret,

			owners_pending_prune: BTreeMap::new(),
			pending_prune: None,
			_deprecated_managed: true,
		});
		OccupiedDistEntry {
			distributions,
			owners,
			idx,
		}
	}
}

enum DistEntry<'d> {
	Vacant(VacantDistEntry<'d>),
	Occupied(OccupiedDistEntry<'d>),
}
impl DistEntry<'_> {
	fn remove(self, whole_dist: bool, reason: String) -> Self {
		match self {
			DistEntry::Vacant(_) => self,
			DistEntry::Occupied(o) => Self::Vacant(o.remove(whole_dist, reason)),
		}
	}
	fn set(self, secret: FleetSecretData, reason: String) -> Self {
		Self::Occupied(match self {
			DistEntry::Vacant(e) => e.set(secret),
			DistEntry::Occupied(e) => e.set(secret, reason),
		})
	}
}

impl Serialize for FleetSecretDistributions {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut v = self.clone();
		v.prune_dead();
		let mut found_hosts = BTreeSet::new();
		for ele in v.distributions() {
			if ele.pending_prune.is_some() {
				continue;
			}
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
		match v.stored.len() {
			0 => panic!("consistency: empty distributions"),
			1 => v.stored[0].serialize(serializer),
			_ => {
				let mut sorted = v.stored.clone();
				// Store outdated distributions last
				sorted.sort_by_key(|v| v.pending_prune.is_some() as u32);
				sorted.serialize(serializer)
			}
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
		let stored = match d {
			Distributions::One(d) => vec![d],
			Distributions::Many(ds) => ds,
		};
		if stored.is_empty() {
			return Err(de::Error::custom("consistency: empty distributions"));
		}
		let mut found_hosts = BTreeSet::new();
		for ele in stored.iter() {
			if ele.pending_prune.is_some() {
				continue;
			}
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
		Ok(Self { stored })
	}
}

#[derive(Deserialize, Default)]
pub struct FleetSecrets(BTreeMap<String, FleetSecretDistributions>);

impl Serialize for FleetSecrets {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let data: BTreeMap<String, FleetSecretDistributions> = self
			.0
			.iter()
			.filter(|(_, v)| !v.stored.is_empty())
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();

		data.serialize(serializer)
	}
}

impl FleetSecrets {
	pub fn keys(&self) -> btree_map::Keys<String, FleetSecretDistributions> {
		self.0.keys()
	}

	pub fn keys_for_owner(&self, owner: &SecretOwner) -> impl Iterator<Item = &String> {
		self.0
			.iter()
			.filter(|(_, d)| d.contains(owner))
			.map(|(n, _)| n)
	}

	pub fn set_data(&mut self, secret: String, data: FleetSecretDistribution) {
		match self.0.entry(secret) {
			Entry::Vacant(e) => {
				e.insert(FleetSecretDistributions { stored: vec![data] });
			}
			Entry::Occupied(mut e) => {
				let dists = e.get_mut();
				dists.extend(data, "secret data was replaced".to_owned())
			}
		}
	}
	pub fn get(&self, secret: &str) -> Option<&FleetSecretDistributions> {
		self.0.get(secret)
	}
	pub fn get_mut(&mut self, secret: &str) -> Option<&mut FleetSecretDistributions> {
		self.0.get_mut(secret)
	}

	pub fn get_or_create(&mut self, secret: &str) -> &mut FleetSecretDistributions {
		self.0
			.entry(secret.to_owned())
			.or_insert(FleetSecretDistributions::default())
	}

	pub fn contains(&self, secret: &str) -> bool {
		self.0.contains_key(secret)
	}
	pub fn remove(&mut self, secret: &str) {
		self.0.remove(secret);
	}

	fn merge_from_hosts(
		&mut self,
		host_secrets: BTreeMap<SecretOwner, BTreeMap<String, FleetSecretDistribution>>,
	) {
		for (host, host_secrets) in host_secrets {
			for (secret_name, mut secret_data) in host_secrets {
				secret_data.owners.insert(host.clone());
				self.set_data(secret_name, secret_data);
			}
		}
	}

	pub fn prune_host(&mut self, host: &SecretOwner, expected_nonshared: BTreeSet<String>) {
		for (name, dists) in self.0.iter_mut() {
			if expected_nonshared.contains(name) {
				continue;
			}
			for dist in dists.distributions_mut() {
				if dist.owners.contains(host) {
					dist.prune_owners(
						&BTreeSet::from([host.to_owned()]),
						"host no longer defines this secret".to_owned(),
					);
				}
			}
		}
	}
}

#[derive(Debug, Clone)]
pub struct Expectations {
	pub owners: BTreeSet<SecretOwner>,
	pub generation_data: serde_json::Value,
	pub parts: BTreeMap<String, GeneratorPart>,
}
#[derive(Deserialize, Debug, Clone)]
pub struct GeneratorPart {
	pub encrypted: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RegenerationConstraints {
	pub allow_different: bool,
	pub regenerate_on_owner_added: bool,
	pub regenerate_on_owner_removed: bool,
}
impl RegenerationConstraints {
	pub fn host_personal() -> Self {
		Self {
			allow_different: false,
			regenerate_on_owner_added: true,
			regenerate_on_owner_removed: true,
		}
	}
	pub fn without_preferences(self) -> Self {
		Self {
			allow_different: self.allow_different,
			regenerate_on_owner_added: false,
			regenerate_on_owner_removed: false,
		}
	}
}
