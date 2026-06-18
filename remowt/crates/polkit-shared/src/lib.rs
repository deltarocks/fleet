use std::collections::HashMap;
use std::{fmt, fs};

use nix::unistd::{Uid, User};
use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedValue, Type, Value};

pub fn emphasize(s: impl AsRef<str>) -> String {
	format!("<span style=\"italic\">&lt;{}&gt;</span>", escape(s),)
}
fn command(s: impl AsRef<str>) -> String {
	format!("<u><tt>{}</tt></u>", s.as_ref())
}
fn escape(s: impl AsRef<str>) -> String {
	s.as_ref()
		.replace("&", "&quot;")
		.replace("<", "&lt;")
		.replace(">", "&gt;")
}

pub struct PidDisplay(pub u32);
impl fmt::Display for PidDisplay {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.0 == 1 {
			emphasize("init").fmt(f)
		} else if let Ok(proc) = fs::read_to_string(format!("/proc/{}/cmdline", self.0)) {
			write!(
				f,
				"command: {}",
				command(
					proc.replace("\0", " ")
						.strip_suffix(" ")
						.expect("cmdline should end with NUL")
				)
			)
		} else if let Ok(proc) = fs::read_to_string(format!("/proc/{}/comm", self.0)) {
			write!(f, "process: {}", command(proc.replace("\0", " ")))
		} else {
			emphasize("unknown process").fmt(f)
		}
	}
}

#[derive(Serialize, Deserialize, Type, PartialEq, Debug)]
pub struct Identity {
	pub kind: String,
	pub details: HashMap<String, OwnedValue>,
}

impl Identity {
	pub fn uid(&self) -> Option<Uid> {
		if self.kind != "unix-user" {
			return None;
		}
		let uid = self.details.get("uid")?;
		let Value::U32(uid) = &**uid else {
			return None;
		};
		Some(Uid::from_raw(*uid))
	}
}

impl fmt::Display for Identity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.kind.as_str() {
			"unix-user" => match self.details.get("uid").map(|v| &**v) {
				Some(Value::U32(uid)) => match User::from_uid(Uid::from_raw(*uid)) {
					Ok(Some(u)) => write!(
						f,
						"user: {} {} {}",
						u.name,
						u.uid,
						if u.gecos.is_empty() {
							"".to_owned()
						} else {
							format!(": {}", escape(u.gecos.to_string_lossy()))
						}
					),
					Ok(None) => emphasize("not found").fmt(f),
					Err(e) => {
						let user = format!("could not get user: {e}");
						emphasize(&user).fmt(f)?;
						Ok(())
					}
				},

				_ => emphasize("unknown uid").fmt(f),
			},
			_ => emphasize(format!("identity of unknown kind: {}", self.kind)).fmt(f),
		}
	}
}

impl Clone for Identity {
	fn clone(&self) -> Self {
		Self {
			kind: self.kind.clone(),
			details: self
				.details
				.iter()
				.map(|(k, v)| (k.clone(), v.try_clone().expect("no fds are expected")))
				.collect(),
		}
	}
}

#[derive(Serialize, Deserialize, Type, PartialEq, Debug)]
pub struct BackendRequest {
	pub cookie: String,
	pub environment: HashMap<String, String>,
	pub prompter_path: String,
	pub identity: Identity,
}
