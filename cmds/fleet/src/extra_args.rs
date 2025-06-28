use std::ffi::{OsStr, OsString};

use anyhow::{Result, anyhow};

pub fn parse_os(os: &OsStr) -> Result<Vec<OsString>> {
	Ok(shlex::bytes::split(os.as_encoded_bytes())
		.ok_or_else(|| anyhow!("invalid arguments"))?
		.into_iter()
		.map(|a| {
			// Unpaired surrogates are not touched
			unsafe { OsString::from_encoded_bytes_unchecked(a) }
		})
		.collect())
}
// pub fn parse(s: &str) -> Result<Vec<OsString>> {
// 	let osstr = OsString::try_from(s)?;
// 	parse_os(&osstr)
// }
