//! Collection of handlers, which transform program-specific stdout format to tracing

use std::{
	collections::HashMap,
	sync::{Arc, LazyLock, Mutex},
};

use regex::Regex;
use serde::Deserialize;
use tracing::{Span, info, info_span, warn};
#[cfg(feature = "indicatif")]
use tracing_indicatif::span_ext::IndicatifSpanExt as _;

pub trait Handler: Send {
	fn handle_line(&mut self, e: &str);
}

/// Handler wrapper, which can be cloned.
pub struct ClonableHandler<H>(Arc<Mutex<H>>);
impl<H> Clone for ClonableHandler<H> {
	fn clone(&self) -> Self {
		Self(self.0.clone())
	}
}
impl<H> ClonableHandler<H> {
	pub fn new(inner: H) -> Self {
		Self(Arc::new(Mutex::new(inner)))
	}
}
impl<H: Handler> Handler for ClonableHandler<H> {
	fn handle_line(&mut self, e: &str) {
		self.0.lock().unwrap().handle_line(e)
	}
}

/// Converts command output to tracing lines
pub struct PlainHandler;
impl Handler for PlainHandler {
	fn handle_line(&mut self, e: &str) {
		info!(target: "log", "{e}");
	}
}

/// Ignores output
pub struct NoopHandler;
impl Handler for NoopHandler {
	fn handle_line(&mut self, _e: &str) {}
}

/// Transform nix internal-json logs to tracing spans.
#[derive(Default)]
pub struct NixHandler {
	spans: HashMap<u64, Span>,
}
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum LogField {
	String(String),
	Num(u64),
}

/// Nix internal-json log line type
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", tag = "action")]
#[allow(dead_code)]
enum NixLog {
	Msg {
		level: u32,
		msg: String,
		raw_msg: Option<String>,
	},
	Start {
		id: u64,
		level: u32,
		#[serde(default)]
		fields: Vec<LogField>,
		text: String,
		#[serde(rename = "type")]
		typ: u32,
	},
	Stop {
		id: u64,
	},
	Result {
		id: u64,
		#[serde(rename = "type")]
		typ: u32,
		#[serde(default)]
		fields: Vec<LogField>,
	},
}
fn process_message(m: &str) -> String {
	// Supposed to remove formatting characters except colors, as some programs try to reset cursor position etc.
	static OSC_CLEANER: LazyLock<Regex> =
		LazyLock::new(|| Regex::new(r"\x1B\]([^\x07\x1C]*[\x07\x1C])?|\r").unwrap());
	static DETABBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\t").unwrap());
	let m = OSC_CLEANER.replace_all(m, "");
	// Indicatif can't format tabs. This is not the correct tab formatting, as correct one should be aligned,
	// and not just be replaced with the constant number of spaces, but it's ok for now, as statuses are single-line.
	DETABBER.replace_all(m.as_ref(), "  ").to_string()
}
impl Handler for NixHandler {
	fn handle_line(&mut self, e: &str) {
		if let Some(e) = e.strip_prefix("@nix ") {
			let log: NixLog = match serde_json::from_str(e) {
				Ok(l) => l,
				Err(err) => {
					warn!("failed to parse nix log line {:?}: {}", e, err);
					return;
				}
			};
			match log {
				NixLog::Msg { msg, raw_msg, .. } => {
					#[allow(clippy::nonminimal_bool)]
					if !(msg.starts_with("\u{1b}[35;1mwarning:\u{1b}[0m Git tree '")
						&& msg.ends_with("' is dirty"))
						&& !msg.starts_with(
							"\u{1b}[35;1mwarning:\u{1b}[0m not writing modified lock file of flake",
						) && msg
						!= "\u{1b}[35;1mwarning:\u{1b}[0m \u{1b}[31;1merror:\u{1b}[0m SQLite database '\u{1b}[35;1m/nix/var/nix/db/db.sqlite\u{1b}[0m' is busy"
					{
						if let Some(raw_msg) = raw_msg {
							if !msg.is_empty() {
								info!(target: "nix", "{}\n{}", raw_msg.trim_end(), msg.trim_end())
							} else {
								info!(target: "nix", "{}", raw_msg.trim_end())
							}
						} else {
							info!(target: "nix", "{}", msg.trim_end())
						}
					}
				}
				NixLog::Start {
					ref fields,
					typ,
					id,
					..
				} if typ == 105 && !fields.is_empty() => {
					if let [LogField::String(drv), ..] = &fields[..] {
						let mut drv = drv.as_str();
						if let Some(pkg) = drv.strip_prefix("/nix/store/") {
							let mut it = pkg.splitn(2, '-');
							it.next();
							if let Some(pkg) = it.next() {
								drv = pkg;
							}
						}
						info!(target: "nix","building {}", drv);
						let span = info_span!("build", drv);
						#[cfg(feature = "indicatif")]
						span.pb_start();
						self.spans.insert(id, span);
					} else {
						warn!("bad build log: {:?}", log)
					}
				}
				NixLog::Start {
					ref fields,
					typ,
					id,
					..
				} if typ == 100 && fields.len() >= 3 => {
					if let [
						LogField::String(drv),
						LogField::String(from),
						LogField::String(to),
						..,
					] = &fields[..]
					{
						let mut drv = drv.as_str();

						if let Some(pkg) = drv.strip_prefix("/nix/store/") {
							let mut it = pkg.splitn(2, '-');
							it.next();
							if let Some(pkg) = it.next() {
								drv = pkg;
							}
						}
						info!(target: "nix","copying {} {} -> {}", drv, from, to);
						let span = info_span!("copy", from, to, drv);
						#[cfg(feature = "indicatif")]
						span.pb_start();
						self.spans.insert(id, span);
					} else {
						warn!("bad copy log: {:?}", log)
					}
				}
				NixLog::Start { text, typ, id, .. }
					if typ == 0 || typ == 102 || typ == 103 || typ == 104 =>
				{
					if !text.is_empty()
						&& text != "querying info about missing paths"
						&& text != "copying 0 paths"
						// Too much spam on lazy-trees branch
						&& !(text.starts_with("copying '") && text.ends_with("' to the store"))
					{
						let span = info_span!("job");
						#[cfg(feature = "indicatif")]
						{
							span.pb_start();
							span.pb_set_message(&process_message(text.trim()));
						}
						self.spans.insert(id, span);
						info!(target: "nix", "{}", text);
					}
				}
				NixLog::Start {
					text,
					level: 0,
					typ: 108,
					..
				} if text.is_empty() => {
					// Cache lookup? Coupled with copy log
				}
				NixLog::Start {
					text,
					level: 4,
					typ: 109,
					..
				} if text.starts_with("querying info about ") => {
					// Cache lookup
				}
				NixLog::Start {
					text,
					level: 4,
					typ: 101,
					..
				} if text.starts_with("downloading ") => {
					// NAR downloading, coupled with copy log
				}
				NixLog::Start {
					text,
					level: 1,
					typ: 111,
					..
				} if text.starts_with("waiting for a machine to build ") => {
					// Useless repeating notification about build
				}
				NixLog::Start {
					text,
					level: 3,
					typ: 111,
					..
				} if text.starts_with("resolved derivation: ") => {
					// CA resolved
				}
				NixLog::Start {
					text,
					level: 1,
					typ: 111,
					id,
					..
				} if text.starts_with("waiting for lock on ") => {
					let mut drv = text.strip_prefix("waiting for lock on ").unwrap();
					if let Some(txt) = drv.strip_prefix("\u{1b}[35;1m'") {
						drv = txt;
					}
					if let Some(txt) = drv.strip_suffix("'\u{1b}[0m") {
						drv = txt;
					}
					if let Some(txt) = drv.split("', '").next() {
						drv = txt;
					}
					if let Some(pkg) = drv.strip_prefix("/nix/store/") {
						let mut it = pkg.splitn(2, '-');
						it.next();
						if let Some(pkg) = it.next() {
							drv = pkg;
						}
					}
					let span = info_span!("waiting on drv", drv);
					#[cfg(feature = "indicatif")]
					span.pb_start();
					self.spans.insert(id, span);
					// Concurrent build of the same message
				}
				NixLog::Stop { id, .. } => {
					self.spans.remove(&id);
				}
				NixLog::Result { fields, id, typ } if typ == 101 && !fields.is_empty() => {
					if let Some(span) = self.spans.get(&id) {
						if let LogField::String(s) = &fields[0] {
							#[cfg(feature = "indicatif")]
							span.pb_set_message(&process_message(s.trim()));
							#[cfg(not(feature = "indicatif"))]
							{
								let _span = span.enter();
								info!("{}", process_message(s));
							}
						} else {
							warn!("bad fields: {fields:?}");
						}
					} else {
						warn!("unknown result id: {id} {typ} {fields:?}");
					}
					// dbg!(fields, id, typ);
				}
				NixLog::Result { fields, id, typ } if typ == 105 && fields.len() >= 4 => {
					if let Some(span) = self.spans.get(&id) {
						if let [
							LogField::Num(done),
							LogField::Num(expected),
							LogField::Num(_running),
							LogField::Num(_failed),
						] = &fields[..4]
						{
							#[cfg(feature = "indicatif")]
							{
								span.pb_set_length(*expected);
								span.pb_set_position(*done);
							}
							let _ = (span, done, expected);
						} else {
							warn!("bad fields: {fields:?}");
						}
					} else {
						// warn!("unknown result id: {id} {typ} {fields:?}");
						// Unaccounted progress.
					}
					// dbg!(fields, id, typ);
				}
				NixLog::Result { typ, .. } if typ == 104 || typ == 106 => {
					// Set phase, expected
				}
				_ => warn!("unknown log: {:?}", log),
			};
		} else {
			let e = e.trim();
			if e.starts_with("Failed tcsetattr(TCSADRAIN): ") {
				return;
			}
			info!("{e}")
		}
	}
}
