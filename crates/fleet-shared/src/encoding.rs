use std::{
	collections::BTreeMap, fmt::{self, Display}, str::FromStr
};

use base64::engine::{Engine, general_purpose::STANDARD_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use unicode_categories::UnicodeCategories;

#[derive(Debug, PartialEq, Clone)]
pub struct SecretData {
	pub data: Vec<u8>,
	pub encrypted: bool,
}

const BASE64_ENCODED_PREFIX: &str = "<BASE64-ENCODED>\n";
const Z85_ENCODED_PREFIX: &str = "<Z85-ENCODED>\n";
// Multiline text in Nix can only end with \n, which is not cool for actual single-line strings.
const PLAINTEXT_NEWLINE_PREFIX: &str = "<PLAINTEXT-NL>\n";
const PLAINTEXT_PREFIX: &str = "<PLAINTEXT>";

const SECRET_PREFIX: &str = "<ENCRYPTED>";

impl<'de> Deserialize<'de> for SecretData {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let string = String::deserialize(deserializer)?;
		string.parse().map_err(D::Error::custom)
	}
}

impl Serialize for SecretData {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.to_string().serialize(serializer)
	}
}

impl FromStr for SecretData {
	type Err = String;

	fn from_str(string: &str) -> Result<Self, Self::Err> {
		let (encrypted, string) = if let Some(unprefixed) = string.strip_prefix(SECRET_PREFIX) {
			(true, unprefixed)
		} else {
			(false, string)
		};
		let data = if let Some(unprefixed) = string.strip_prefix(BASE64_ENCODED_PREFIX) {
			STANDARD_NO_PAD
				.decode(unprefixed.replace(['\n', '\t', ' '], ""))
				.map_err(|e| format!("base64-encoded failed: {e}"))?
		} else if let Some(unprefixed) = string.strip_prefix(Z85_ENCODED_PREFIX) {
			z85::decode(unprefixed.replace(['\n', '\t', ' '], ""))
				.map_err(|e| format!("z85-encoded failed: {e}"))?
		} else if let Some(unprefixed) = string.strip_prefix(PLAINTEXT_NEWLINE_PREFIX) {
			unprefixed.as_bytes().to_owned()
		} else if let Some(unprefixed) = string.strip_prefix(PLAINTEXT_PREFIX) {
			unprefixed.as_bytes().to_owned()
		} else {
			let secret_prefix = format!("{SECRET_PREFIX}{Z85_ENCODED_PREFIX}");
			return Err(format!(
				"unknown secret encoding. If you're migrating from old version of fleet, prefix public secret fields with {PLAINTEXT_PREFIX:?}, and encrypted data with {secret_prefix:?}: {string}"
			));
		};
		Ok(Self { data, encrypted })
	}
}

impl Display for SecretData {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let mut readable = std::str::from_utf8(&self.data).ok();
		if self.encrypted {
			write!(f, "{SECRET_PREFIX}")?;
			// Always base64-encode encrypted fields.
			readable = None;
		}
		if Some(false) == readable.map(is_printable) {
			readable = None
		};
		// TODO: Check if text is readable, and has no unprintable characters?..
		if let Some(plaintext) = readable {
			if plaintext.ends_with('\n') {
				write!(f, "{PLAINTEXT_NEWLINE_PREFIX}")?;
			} else {
				write!(f, "{PLAINTEXT_PREFIX}")?;
			}
			write!(f, "{plaintext}")?;
		} else {
			write!(f, "{BASE64_ENCODED_PREFIX}")?;
			let encoded = STANDARD_NO_PAD.encode(&self.data);
			for ele in encoded.as_bytes().chunks(64) {
				let chunk = std::str::from_utf8(ele).expect(
					"any slice of base64-encoded text is utf-8 compatible, as it is ascii-based",
				);
				writeln!(f, "{chunk}")?;
			}
		};
		Ok(())
	}
}

fn is_printable(text: &str) -> bool {
	text.chars().all(|c| {
		c.is_letter()
			|| c.is_mark()
			|| c.is_number()
			|| c.is_punctuation()
			|| c.is_separator()
			|| c == '\n' || c == '\t'
			// Complete base64 alphabet
			|| c == '/' || c == '+'
			|| c == '='
	})
}

#[test]
fn test() {
	fn check_roundtrip(data: SecretData, expected: &str) {
		let string = data.to_string();
		assert_eq!(string, expected, "unexpected encoding");
		let roundtrip: SecretData = string.parse().expect("roundtrip parse");
		assert_eq!(data, roundtrip, "roundtrip didn't match");
	}
	check_roundtrip(
		SecretData {
			data: vec![1, 2, 3, 4, 5, 6],
			encrypted: false,
		},
		"<BASE64-ENCODED>\nAQIDBAUG\n",
	);
	check_roundtrip(
		SecretData {
			data: vec![1, 2, 3, 4, 5, 6],
			encrypted: true,
		},
		"<ENCRYPTED><BASE64-ENCODED>\nAQIDBAUG\n",
	);
	check_roundtrip(
		SecretData {
			data: "Привет, мир!\n".to_owned().into(),
			encrypted: false,
		},
		"<PLAINTEXT-NL>\nПривет, мир!\n",
	);
	check_roundtrip(
		SecretData {
			data: "Привет, мир!".to_owned().into(),
			encrypted: false,
		},
		"<PLAINTEXT>Привет, мир!",
	);
}
