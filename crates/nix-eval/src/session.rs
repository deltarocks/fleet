use std::{ffi::OsStr, num::ParseIntError, process::Stdio, sync::Arc};

use better_command::{ClonableHandler, Handler, NixHandler, NoopHandler};
use futures::StreamExt;
use itertools::Itertools as _;
use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
	io::AsyncWriteExt,
	process::{ChildStderr, ChildStdin, ChildStdout, Command},
	select,
	sync::{Mutex, mpsc, oneshot},
};
use tokio_util::codec::{FramedRead, LinesCodec};
use tracing::{Level, debug, error, warn};

#[derive(Error, Debug, Clone)]
pub enum Error {
	#[error("failed to create nix repl session: {0}")]
	SessionInit(&'static str),
	#[error("unexpected end of output, nix crashed?")]
	MissingDelimiter,

	#[error("expression did'nt produce any output")]
	ExpectedOutput,
	#[error("expression produced output, which is unexpected")]
	UnexpectedOutput,

	#[error("unexpected expression output type")]
	InvalidType,

	#[error("failed to build attr {attribute}:\n{error}")]
	BuildFailed { attribute: String, error: String },

	#[error("output: {0}")]
	Json(Arc<serde_json::Error>),
	// int outputs are too specific, and should not be used,
	// thus error is ok to be not informative.
	#[error("int output: {0}")]
	Int(ParseIntError),
	#[error("pool: {0}")]
	Pool(Arc<r2d2::Error>),
	#[error("io: {0}")]
	Io(Arc<std::io::Error>),

	// TODO: Should be done by wrapper/in different type.
	#[error("at {0}: {1}")]
	InContext(String, Box<Self>),

	#[error("error: {0}")]
	NixError(String),
}
impl From<r2d2::Error> for Error {
	fn from(value: r2d2::Error) -> Self {
		Self::Pool(Arc::new(value))
	}
}
impl From<std::io::Error> for Error {
	fn from(value: std::io::Error) -> Self {
		Self::Io(Arc::new(value))
	}
}
impl From<serde_json::Error> for Error {
	fn from(value: serde_json::Error) -> Self {
		Self::Json(Arc::new(value))
	}
}
impl Error {
	pub(crate) fn context(self, context: String) -> Self {
		Self::InContext(context, Box::new(self))
	}
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

enum OutputLine {
	Out(String),
	Err(String),
}
struct OutputHandler {
	rx: mpsc::Receiver<OutputLine>,
	_cancel_handle: oneshot::Receiver<()>,
}
impl OutputHandler {
	fn new(out: ChildStdout, err: ChildStderr) -> Self {
		let mut out = FramedRead::new(out, LinesCodec::new());
		let mut err = FramedRead::new(err, LinesCodec::new());
		let (tx, rx) = mpsc::channel(20);
		let (mut cancelled, _cancel_handle) = oneshot::channel();
		tokio::spawn(async move {
			loop {
				select! {
					// We should receive errors earlier than synchronization
					biased;
					e = err.next() => {
						let Some(Ok(e)) = e else {
							if e.is_some() {
								error!("bad repl stderr: {e:?}");
							}
							continue;
						};
						let _ = tx.send(OutputLine::Err(e)).await;
					}
					o = out.next() => {
						let Some(Ok(o)) = o else {
							if o.is_some() {
								error!("bad repl stdout: {o:?}");
							}
							continue;
						};
						let _ = tx.send(OutputLine::Out(o)).await;
					}
					// Reader doesn't care about stdout, as this is cancelled.
					// Error still might be useful, to process leftover span closures?
					_ = cancelled.closed() => {
						break;
					}
				}
			}
		});
		Self { rx, _cancel_handle }
	}
	async fn next(&mut self) -> Option<OutputLine> {
		self.rx.recv().await
	}
}

#[must_use]
struct ErrorCollector<'i, H> {
	collected: Vec<String>,
	inner: &'i mut H,
}
impl<'i, H> ErrorCollector<'i, H> {
	fn new(inner: &'i mut H) -> Self {
		Self {
			collected: vec![],
			inner,
		}
	}
}
impl<H> ErrorCollector<'_, H> {
	fn handle_line_inner(&mut self, msg: &str) -> bool {
		let Some(msg) = msg.strip_prefix("@nix ") else {
			return false;
		};
		#[derive(Deserialize)]
		struct ErrorAction {
			action: String,
			level: u32,
			msg: String,
		}
		let Ok(act) = serde_json::from_str::<ErrorAction>(msg) else {
			return false;
		};
		if act.action != "msg" || act.level != 0 {
			return false;
		}
		self.collected.push(act.msg);
		true
	}
	fn finish(self) -> Result<()> {
		// fn dedent(s: String) -> String {
		// 	s.split('\n').filter(|s| !s.trim().is_empty()).map(|v| v.)
		// }
		if !self.collected.is_empty() {
			return Err(Error::NixError(
				self.collected
					.iter()
					.map(|v| {
						if let Some(f) = v.strip_prefix("\u{1b}[31;1merror:\u{1b}[0m ") {
							let v = unindent::unindent(f.trim_start());
							v.trim().to_owned()
						} else {
							v.to_owned()
						}
					})
					.join("\n")
					.to_string(),
			));
		}
		Ok(())
	}
	fn flush(self) {
		for line in self.collected {
			warn!("{line}");
		}
	}
}
impl<H: Handler> Handler for ErrorCollector<'_, H> {
	fn handle_line(&mut self, e: &str) {
		if self.handle_line_inner(e) {
			return;
		}
		self.inner.handle_line(e)
	}
}

pub struct NixSessionInner {
	full_delimiter: String,
	nix_handler: ClonableHandler<NixHandler>,
	out: OutputHandler,
	stdin: ChildStdin,
	string_wrapping: (String, String),
	number_wrapping: (String, String),

	executing_command: Arc<Mutex<()>>,

	next_id: u32,
	pub(crate) free_list: Vec<u32>,

	pub nix_system: String,
}

/// Discover inter-message repl delimiter
const REPL_DELIMITER: &str = "\"FLEET_MAGIC_REPL_DELIMITER\"";
/// Discover formatting around strings
const TRAIN_STRING: &str = "\"TRAIN_STRING\"";
/// Discover formatting around numbers
const TRAIN_NUMBER: &str = "13141516";
// Other types of formatting are not discovered, because they are not used, JSON serialization is used instead
// Techically, number training is also not required, because numbers can be converted to string too...
// Eh, I'll remove it later.

impl NixSessionInner {
	pub(crate) async fn new(
		flake: &OsStr,
		extra_args: impl IntoIterator<Item = &OsStr>,
		nix_system: String,
		fail_fast: bool,
	) -> Result<Self> {
		let mut cmd = Command::new("nix");
		cmd.arg("repl")
			.args(["--option", "pure-eval", "true"])
			.arg(flake)
			.arg("--log-format")
			.arg("internal-json");
		if !fail_fast {
			cmd.arg("--keep-going");
		}
		for arg in extra_args {
			cmd.arg(arg);
		}
		cmd.stdin(Stdio::piped());
		cmd.stdout(Stdio::piped());
		cmd.stderr(Stdio::piped());
		let cmd = cmd.spawn()?;
		let stdout = cmd.stdout.unwrap();
		let stderr = cmd.stderr.unwrap();
		let mut out = OutputHandler::new(stdout, stderr);
		let mut stdin = cmd.stdin.unwrap();
		// Standard repl hello doesn't work with internal-json logger
		stdin.write_all(REPL_DELIMITER.as_bytes()).await?;
		stdin.write_all(b"\n").await?;
		stdin.flush().await?;
		let nix_handler = NixHandler::default();
		let mut full_delimiter = None;
		let mut errors = vec![];
		while let Some(line) = out.next().await {
			let line = match line {
				OutputLine::Out(o) => o,
				OutputLine::Err(_e) => {
					// Handle startup errors, but skip repl hello?
					errors.push(_e);
					continue;
				}
			};
			if line.contains(REPL_DELIMITER) {
				debug!("discovered repl delimiter with added colors: {line}");
				full_delimiter = Some(line.to_owned());
				break;
			}
		}
		let Some(full_delimiter) = full_delimiter else {
			for e in errors {
				error!("{e}");
			}
			return Err(Error::SessionInit("failed to discover delimiter"));
		};
		let mut res = Self {
			full_delimiter,
			nix_handler: ClonableHandler::new(nix_handler),
			out,
			stdin,
			string_wrapping: Default::default(),
			number_wrapping: Default::default(),

			executing_command: Arc::new(Mutex::new(())),

			next_id: 0,
			free_list: vec![],

			nix_system,
		};
		res.train().await?;
		Ok(res)
	}
	async fn train(&mut self) -> Result<()> {
		{
			let full_string = self
				.execute_expression_raw(TRAIN_STRING, &mut NoopHandler)
				.await?;
			let string_offset = full_string.find(TRAIN_STRING).expect("contained");
			let string_prefix = &full_string[..string_offset];
			let string_suffix = &full_string[string_offset + TRAIN_STRING.len()..];
			self.string_wrapping = (string_prefix.to_owned(), string_suffix.to_owned());
		}
		{
			let full_number = self
				.execute_expression_raw(TRAIN_NUMBER, &mut NoopHandler)
				.await?;
			let number_offset = full_number.find(TRAIN_NUMBER).expect("contained");
			let number_prefix = &full_number[..number_offset];
			let number_suffix = &full_number[number_offset + TRAIN_NUMBER.len()..];
			self.number_wrapping = (number_prefix.to_owned(), number_suffix.to_owned());
		}
		Ok(())
	}
	async fn send_command(&mut self, cmd: impl AsRef<[u8]>) -> Result<()> {
		if tracing::enabled!(Level::DEBUG) && cmd.as_ref() != REPL_DELIMITER.as_bytes() {
			let cmd_str = String::from_utf8_lossy(cmd.as_ref());
			tracing::debug!("{cmd_str}");
		};
		self.stdin.write_all(cmd.as_ref()).await?;
		self.stdin.write_all(b"\n").await?;
		Ok(())
	}
	async fn read_until_delimiter(&mut self, err_handler: &mut dyn Handler) -> Result<String> {
		let mut out = String::new();
		while let Some(line) = self.out.next().await {
			let line = match line {
				OutputLine::Out(out) => out,
				OutputLine::Err(err) => {
					err_handler.handle_line(&err);
					continue;
				}
			};
			if line == self.full_delimiter {
				return Ok(out);
			}
			if !out.is_empty() {
				out.push('\n');
			}
			out.push_str(&line);
		}
		Err(Error::MissingDelimiter)
	}
	pub(crate) async fn execute_expression_number(
		&mut self,
		expr: impl AsRef<[u8]>,
	) -> Result<u64> {
		let num = self.number_wrapping.clone();
		let n = self.execute_expression_wrapping(expr, &num).await?;
		n.parse::<u64>().map_err(Error::Int)
	}
	async fn execute_expression_string(&mut self, expr: impl AsRef<[u8]>) -> Result<String> {
		// builtins.toJSON escapes some thing in incorrect way, e.g escaped "$" in "\${" is being outputed as "\$",
		// while this escape should be removed as it is intended for nix itself, not for json output.
		//
		// This regex only allows \$ in the beginning of the string, it is easier to implement correctly.
		// TODO: Add peg parser for nix-produced JSON?..
		let regex = regex::Regex::new(r#"(?<prefix>[: {,\[]\\")\\\$"#).expect("fixup json");

		let num = self.string_wrapping.clone();
		let n = self.execute_expression_wrapping(expr, &num).await?;
		let n = regex.replace_all(&n, "$prefix$$");
		let str: String = serde_json::from_str(&n)?;
		Ok(str)
	}
	pub(crate) async fn execute_expression_to_json<V: DeserializeOwned>(
		&mut self,
		expr: impl AsRef<[u8]>,
	) -> Result<V> {
		let mut fexpr = b"builtins.toJSON (".to_vec();
		fexpr.extend_from_slice(expr.as_ref());
		fexpr.push(b')');

		Ok(serde_json::from_str(
			&self.execute_expression_string(fexpr).await?,
		)?)
	}
	async fn execute_expression_wrapping(
		&mut self,
		expr: impl AsRef<[u8]>,
		wrapping: &(String, String),
	) -> Result<String> {
		let mut nix_handler = self.nix_handler.clone();
		let mut collected = ErrorCollector::new(&mut nix_handler);
		let res = self.execute_expression_raw(expr, &mut collected).await?;
		if res.is_empty() {
			collected.finish()?;
			return Err(Error::ExpectedOutput);
		} else {
			collected.flush()
		};
		let Some(res) = res.strip_prefix(&wrapping.0) else {
			return Err(Error::InvalidType);
		};
		let Some(res) = res.strip_suffix(&wrapping.1) else {
			return Err(Error::InvalidType);
		};
		Ok(res.to_owned())
	}
	async fn execute_expression_empty(&mut self, expr: impl AsRef<[u8]>) -> Result<()> {
		let mut nix_handler = self.nix_handler.clone();
		let mut collected = ErrorCollector::new(&mut nix_handler);
		let v = self.execute_expression_raw(expr, &mut collected).await?;
		collected.finish()?;
		if !v.is_empty() {
			return Err(Error::UnexpectedOutput);
		}
		Ok(())
	}
	pub(crate) async fn execute_expression_raw(
		&mut self,
		expr: impl AsRef<[u8]>,
		err_handler: &mut dyn Handler,
	) -> Result<String> {
		// Prevent two commands from being executed in parallel, messing with each other.
		let _lock = self.executing_command.clone();
		let _guard = _lock.lock().await;

		self.send_command(expr).await?;
		// It will be echoed
		self.send_command(REPL_DELIMITER).await?;
		self.read_until_delimiter(err_handler).await
	}
	pub(crate) async fn execute_assign(&mut self, expr: impl AsRef<str>) -> Result<u32> {
		let id = self.allocate_id();
		self.execute_expression_empty(format!("sess_field_{id} = {}", expr.as_ref()))
			.await?;
		Ok(id)
	}

	/// Id should be immediately used
	fn allocate_id(&mut self) -> u32 {
		if let Some(free) = self.free_list.pop() {
			free
		} else {
			let v = self.next_id;
			self.next_id += 1;
			v
		}
	}
	// Nix has no way to deallocate variable, yet GC will correct everything not reachable.
	// async fn free_id(&mut self, id: u32) -> Result<()> {
	// 	self.execute_expression_empty(format!("sess_field_{id} = null"))
	// 		.await?;
	// 	self.free_list.push(id);
	// 	Ok(())
	// }
}
