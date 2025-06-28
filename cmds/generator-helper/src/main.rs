use std::{
	env,
	fs::{File, OpenOptions},
	io::{self, Read, Write, copy, stdin, stdout},
	str::FromStr,
};

use age::{
	Encryptor, Recipient,
	ssh::{ParseRecipientKeyError, Recipient as SshRecipient},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, ValueEnum};
use ed25519_dalek::SecretKey;
use fleet_shared::SecretData;
use rand::{
	RngCore,
	distr::{Alphanumeric, Distribution, SampleString, Uniform},
	rng,
};

fn write_output_file(out: &str) -> Result<File> {
	let file = OpenOptions::new()
		.create_new(true)
		.write(true)
		.open(out)
		.with_context(|| format!("failed to open output {out:?}"))?;
	Ok(file)
}
fn write_public(out: &str, mut input: impl Read, encoding: OutputEncoding) -> Result<()> {
	let mut output = write_output_file(out)?;

	let mut data = Vec::new();
	copy(&mut input, &mut wrap_encoder(&mut data, encoding))?;

	output.write_all(
		SecretData {
			data,
			encrypted: false,
		}
		.to_string()
		.as_bytes(),
	)?;
	Ok(())
}
fn write_private(
	identities: &Identities,
	out: &str,
	mut input: impl Read,
	encoding: OutputEncoding,
) -> Result<()> {
	let mut output = write_output_file(out)?;
	let encryptor = make_encryptor(identities)?;

	let mut data = Vec::new();
	{
		let mut encrypted_writer = encryptor.wrap_output(&mut data)?;
		copy(
			&mut input,
			&mut wrap_encoder(&mut encrypted_writer, encoding),
		)?;
		encrypted_writer.finish()?;
	};

	output.write_all(
		SecretData {
			data,
			encrypted: true,
		}
		.to_string()
		.as_bytes(),
	)?;
	Ok(())
}

type Identities = Vec<SshRecipient>;
fn load_identities() -> Result<Identities> {
	let list = env::var("GENERATOR_HELPER_IDENTITIES");
	let list = match list {
		Ok(v) => v,
		Err(env::VarError::NotPresent) => {
			bail!(
				"gh is only intended to be used from secret generator scripts, but if you really want to use it somewhere else - set GENERATOR_HELPER_IDENTITIES to list of newline-delimited ssh identities"
			);
		}
		Err(e) => bail!("somehow, identities list is not utf-8: {e}"),
	};
	let list = list.trim();
	ensure!(!list.is_empty(), "no identities passed, can't encrypt data");
	list.lines()
		.map(age::ssh::Recipient::from_str)
		.collect::<Result<Identities, ParseRecipientKeyError>>()
		.map_err(|e| anyhow!("parse recipients: {e:?}"))
}
fn make_encryptor(r: &Identities) -> Result<Encryptor> {
	Ok(
		Encryptor::with_recipients(r.iter().map(|v| v as &dyn Recipient))
			.expect("list is not empty"),
	)
}
fn wrap_encoder<'t>(w: impl Write + 't, encoding: OutputEncoding) -> impl Write + 't {
	fn coerce<'t>(w: impl Write + 't) -> Box<dyn Write + 't> {
		Box::new(w)
	}
	match encoding {
		OutputEncoding::Raw => coerce(w),
		OutputEncoding::Base64 => {
			use base64::{engine::general_purpose::STANDARD, write::EncoderWriter};

			let writer = EncoderWriter::new(w, &STANDARD);
			coerce(writer)
		}
		OutputEncoding::Hex => {
			struct HexWriter<W>(W);
			impl<W> Write for HexWriter<W>
			where
				W: Write,
			{
				fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
					let encoded = hex::encode(buf);
					self.0.write_all(encoded.as_bytes())?;
					Ok(buf.len())
				}

				fn flush(&mut self) -> io::Result<()> {
					self.0.flush()
				}
			}
			coerce(HexWriter(w))
		}
	}
}

#[derive(Clone, Copy, ValueEnum, Default)]
enum OutputEncoding {
	/// Do not encode data, store as is.
	#[default]
	Raw,
	/// Encode as base64 (with padding).
	Base64,
	/// Encode as hex (without leading 0x)
	Hex,
}

#[derive(Parser)]
enum Generate {
	/// Generate public, private keys without wrapping, in standard ed25519 schema
	/// (64 bytes private (due to merge with private), 32 bytes public)
	Ed25519 {
		#[arg(long, short = 'p')]
		public: String,
		#[arg(long, short = 's')]
		private: String,
		/// Private key should be just the private key (32 bytes), not standard private+public.
		#[arg(long)]
		no_embed_public: bool,
		#[arg(long, short = 'e', value_enum, default_value_t)]
		encoding: OutputEncoding,
	},
	/// Generate public, private keys without wrapping, in standard x25519 schema
	/// (32 bytes private, 32 bytes public)
	X25519 {
		#[arg(long, short = 'p')]
		public: String,
		#[arg(long, short = 's')]
		private: String,
		#[arg(long, short = 'e', value_enum, default_value_t)]
		encoding: OutputEncoding,
	},
	Password {
		#[arg(long, short = 'o')]
		output: String,
		#[arg(long)]
		size: usize,
		#[arg(long, short = 'n')]
		no_symbols: bool,
		#[arg(long, short = 'e', value_enum, default_value_t)]
		encoding: OutputEncoding,
	},
	Bytes {
		#[arg(long, short = 'o')]
		output: String,
		#[arg(long, short = 'c')]
		count: usize,
		/// Ensure there is no NULs in bytestring.
		#[arg(long)]
		no_nuls: bool,
		#[arg(long, short = 'e', value_enum, default_value_t)]
		encoding: OutputEncoding,
	},
}

#[derive(Parser)]
enum Opts {
	/// Encode public part from stdin.
	Public {
		#[arg(long, short = 'o')]
		output: String,
		#[arg(long, short = 'e', value_enum, default_value_t)]
		encoding: OutputEncoding,
	},
	/// Encrypt private part from stdin.
	Private {
		#[arg(long, short = 'o')]
		output: String,
		#[arg(long, short = 'e', value_enum, default_value_t)]
		encoding: OutputEncoding,
	},
	/// Sometimes you also need to reencode secret, this command allows you to get raw data from
	/// secret encoded using `gh public`... I would like if I knew a better design for some sort of
	/// such thing. Ideally there should be no need to decode secrets back, but garage wants both
	/// raw pubkey and raw secret key, yet also requires node id which is hex-reencoded public key.
	Decode {
		#[arg(long, short = 'i')]
		input: String,
	},
	/// Generate keys in well-known schemas.
	///
	/// Note that this command is only intended to be used in fleet secret generator,
	/// otherwise you should ensure noone is able to read generated files, they don't have any mode set by default.
	///
	/// Fleet also doesn't zeroize memory/assumes good OsRng/makes other assumptions, which makes it only suitable to
	/// be used in nix sandbox.
	#[command(subcommand)]
	Generate(Generate),
}

fn main() -> Result<()> {
	let opts = Opts::parse();
	// Assumed to be secure, seeded from secure OsRng+reseeded.
	let mut rng = rng();

	match opts {
		Opts::Public { output, encoding } => {
			write_public(&output, stdin(), encoding)?;
		}
		Opts::Private { output, encoding } => {
			let recipients = load_identities()?;
			write_private(&recipients, &output, stdin(), encoding)?;
		}
		Opts::Generate(generate) => {
			match generate {
				Generate::Ed25519 {
					public,
					private,
					no_embed_public,
					encoding,
				} => {
					use ed25519_dalek::SigningKey;

					let recipients = load_identities()?;
					let mut secret = SecretKey::default();
					rng.fill_bytes(&mut secret);
					// TODO: Use SigningKey::generate after https://github.com/dalek-cryptography/curve25519-dalek/pull/762
					let key = SigningKey::from_bytes(&secret).to_keypair_bytes();
					write_public(&public, &key[32..], encoding)?;
					write_private(
						&recipients,
						&private,
						&key[..{ if no_embed_public { 32 } else { 64 } }],
						encoding,
					)?;
				}
				Generate::X25519 {
					public,
					private,
					encoding,
				} => {
					use x25519_dalek::{PublicKey, StaticSecret};

					let recipients = load_identities()?;
					// TODO: Use random_from_rng after https://github.com/dalek-cryptography/curve25519-dalek/pull/762
					let key = StaticSecret::random();
					let public_key: PublicKey = (&key).into();
					write_public(&public, public_key.as_bytes().as_slice(), encoding)?;
					write_private(&recipients, &private, key.as_bytes().as_slice(), encoding)?;
				}
				Generate::Password {
					size,
					no_symbols,
					output,
					encoding,
				} => {
					ensure!(
						size >= 6,
						"misconfiguration? password is shorter than 6 chars"
					);
					let recipients = load_identities()?;
					let out = if no_symbols {
						Alphanumeric.sample_string(&mut rng, size)
					} else {
						// Alphabet of Alphanumberic + symbols
						const GEN_ASCII_SYMBOLS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
						let uniform =
							Uniform::new(0, GEN_ASCII_SYMBOLS.len()).expect("range is valid");
						(0..size)
							.map(|_| uniform.sample(&mut rng))
							.map(|i| GEN_ASCII_SYMBOLS[i] as char)
							.collect::<String>()
					};
					write_private(&recipients, &output, out.as_bytes(), encoding)?;
				}
				Generate::Bytes {
					output,
					count,
					no_nuls,
					encoding,
				} => {
					ensure!(
						count >= 6,
						"misconfiguration? bytestring is shorter than 6 chars"
					);
					let recipients = load_identities()?;
					let mut bytes = vec![0u8; count];
					if no_nuls {
						let rand = Uniform::new_inclusive(0x1u8, 0xffu8)
							.expect("range is valid")
							.sample_iter(&mut rng);
						for (byte, rand) in bytes.iter_mut().zip(rand) {
							*byte = rand;
						}
					} else {
						rng.fill_bytes(&mut bytes);
					};
					write_private(&recipients, &output, bytes.as_slice(), encoding)?;
				}
			}
		}
		Opts::Decode { input } => {
			let mut data = Vec::new();
			File::open(input)?.read_to_end(&mut data)?;
			let data = String::from_utf8(data).context(
				"encoded data is always utf-8, you are trying to use decode the wrong way.",
			)?;
			let data =
				SecretData::from_str(&data).map_err(|e| anyhow!("failed to decode data: {e}"))?;
			ensure!(
				!data.encrypted,
				"you can not decrypt secret data, only decode public."
			);
			stdout().write_all(&data.data)?;
		}
	}
	Ok(())
}
