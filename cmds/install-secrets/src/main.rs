use std::{
	collections::{BTreeMap, HashMap},
	fs::{self, File},
	io::{self, Cursor, Read, Write},
	iter,
	os::unix::prelude::PermissionsExt,
	path::{Path, PathBuf},
	str::{FromStr, from_utf8},
};

use age::{
	Decryptor, Encryptor, Identity, Recipient,
	ssh::{Identity as SshIdentity, Recipient as SshRecipient},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::Parser;
use fleet_shared::SecretData;
use nix::unistd::{Group, User, chown};
use serde::Deserialize;
use tracing::{error, info, info_span};
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

#[derive(Parser)]
#[clap(author)]
enum Opts {
	/// Install secrets from json specification
	Install { data: PathBuf },
	/// Reencrypt secret using host key, outputting in fleet encoded string
	Reencrypt {
		#[clap(long)]
		secret: SecretData,
		#[clap(long)]
		targets: Vec<String>,
	},
	/// Decrypt secret using host key, outputting in fleet encoded string
	Decrypt {
		#[clap(long)]
		secret: SecretData,
		/// Shoult decoded output be printed as plaintext, instead of z85?
		#[clap(long)]
		plaintext: bool,
	},
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part {
	raw: SecretData,
	path: PathBuf,
	stable_path: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataItem {
	group: String,
	mode: String,
	owner: String,
	root_path: Option<PathBuf>,

	#[serde(flatten)]
	parts: BTreeMap<String, Part>,
}

type Data = HashMap<String, DataItem>;

fn decrypt(input: &SecretData, identity: &dyn Identity) -> Result<Vec<u8>> {
	ensure!(input.encrypted, "passed data is not encrypted!");
	let mut input = Cursor::new(&input.data);
	let decryptor = Decryptor::new(&mut input).context("failed to init decryptor")?;
	if decryptor.is_scrypt() {
		bail!("should be recipients");
	}
	let mut decryptor = decryptor
		.decrypt(iter::once(identity as &dyn age::Identity))
		.context("failed to decrypt, wrong key?")?;

	let mut decrypted = Vec::new();
	decryptor
		.read_to_end(&mut decrypted)
		.context("failed to decrypt")?;
	Ok(decrypted)
}
fn encrypt(input: &[u8], targets: Vec<String>) -> Result<SecretData> {
	let recipients = targets
		.into_iter()
		.map(|t| {
			SshRecipient::from_str(&t).map_err(|e| anyhow!("failed to parse recipient: {e:?}"))
		})
		.collect::<Result<Vec<SshRecipient>>>()?;
	let recipients = recipients.iter().map(|v| v as &dyn Recipient);
	let mut encrypted = vec![];
	let mut encryptor = Encryptor::with_recipients(recipients)
		.expect("recipients provided")
		.wrap_output(&mut encrypted)
		.expect("constructor should not fail");
	io::copy(&mut Cursor::new(input), &mut encryptor).expect("copy should not fail");
	encryptor.finish().context("failed to finish encryption")?;
	Ok(SecretData {
		data: encrypted,
		encrypted: true,
	})
}

fn init_part(identity: &dyn Identity, item: &DataItem, value: &Part) -> Result<()> {
	let stable_dir = value.stable_path.parent().expect("not root");

	// Right now stable & non-stable data are both located in this dir.
	std::fs::create_dir_all(stable_dir)?;

	let mut stable_temp =
		tempfile::NamedTempFile::new_in(stable_dir).context("failed to create tempfile")?;
	let mut hashed = File::create(&value.path)?;

	let private = value.raw.encrypted;
	let data = if private {
		decrypt(&value.raw, identity)?
	} else {
		value.raw.data.to_owned()
	};

	hashed.write_all(&data)?;
	hashed.flush()?;
	stable_temp.write_all(&data)?;
	stable_temp.flush()?;

	let mode = if private {
		fs::Permissions::from_mode(
			u32::from_str_radix(&item.mode, 8).context("failed to parse mode as octal")?,
		)
	} else {
		fs::Permissions::from_mode(0o444)
	};
	fs::set_permissions(stable_temp.path(), mode.clone()).context("stable temp mode")?;
	fs::set_permissions(&value.path, mode).context("hashed mode")?;

	// Files are initially owned by root, thus making set mode first inaccessible to user, and then
	// altering user/group.
	if private {
		let user = User::from_name(&item.owner)
			.context("failed to get user")?
			.ok_or_else(|| anyhow!("user not found"))?;
		let group = Group::from_name(&item.group)
			.context("failed to get group")?
			.ok_or_else(|| anyhow!("group not found"))?;

		chown(stable_temp.path(), Some(user.uid), Some(group.gid))
			.context("failed to apply user/group")?;
		chown(&value.path, Some(user.uid), Some(group.gid))
			.context("failed to apply user/group")?;
	}

	stable_temp
		.persist(&value.stable_path)
		.context("stable persist")?;
	Ok(())
}

fn init_secret(identity: &age::ssh::Identity, value: &DataItem) -> Result<()> {
	if let Some(root_path) = &value.root_path {
		if !fs::metadata(root_path).map(|m| m.is_dir()).unwrap_or(false) {
			fs::create_dir(root_path).context("failed to create secret directory")?;
		}
	}
	let mut errored = false;
	for (part_id, part) in value.parts.iter() {
		let _span = info_span!("part", part_id = part_id);
		if let Err(e) = init_part(identity, value, part) {
			error!("failed to init part {part_id:?}: {e}");
			errored = true;
		}
	}

	ensure!(!errored, "some secret parts have failed to initialize");
	Ok(())
}

fn host_identity() -> anyhow::Result<SshIdentity> {
	let identity = SshIdentity::from_buffer(
		&mut Cursor::new(
			fs::read("/etc/ssh/ssh_host_ed25519_key").context("failed to read host private key")?,
		),
		None,
	)
	.context("failed to parse identity")?;
	Ok(identity)
}

fn install(data: &Path) -> anyhow::Result<()> {
	let data = fs::read(data).context("failed to read secrets data")?;
	let data_str = from_utf8(&data).context("failed to read data to string")?;
	let data: Data = serde_json::from_str(data_str).context("failed to parse data")?;

	if !fs::metadata("/run/secrets")
		.map(|m| m.is_dir())
		.unwrap_or(false)
	{
		fs::create_dir("/run/secrets").context("failed to create secrets directory")?;
	}

	if data.is_empty() {
		info!("no secrets to install");
		return Ok(());
	}

	let identity = host_identity()?;

	let mut failed = false;
	for (name, value) in data {
		let _span = info_span!("init", name = name);
		if let Err(e) = init_secret(&identity, &value) {
			error!("secret {name:?} failed to initialize: {e}");
			failed = true;
		}
	}
	if failed {
		bail!("one or more secrets failed");
	}

	Ok(())
}

fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy(),
		)
		.without_time()
		.with_target(false)
		.init();

	let opts = Opts::parse();

	match opts {
		Opts::Install { data } => install(&data),
		Opts::Reencrypt { secret, targets } => {
			let identity = host_identity()?;
			let decrypted = decrypt(&secret, &identity).context("during decryption")?;
			let encrypted = encrypt(&decrypted, targets).context("during re-encryption")?;

			println!("{encrypted}");
			Ok(())
		}
		Opts::Decrypt { secret, plaintext } => {
			let identity = host_identity()?;
			let decrypted = decrypt(&secret, &identity).context("during decryption")?;

			if plaintext {
				let s = String::from_utf8(decrypted).context("output is not utf8")?;
				print!("{s}");
			} else {
				println!(
					"{}",
					SecretData {
						data: decrypted,
						encrypted: false
					}
				);
			}
			Ok(())
		}
	}
}
