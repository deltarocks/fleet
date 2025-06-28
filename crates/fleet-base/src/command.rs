use std::{ffi::OsStr, pin, process::Stdio, sync::Arc, task::Poll};

use anyhow::{Result, anyhow};
use better_command::{Handler, NixHandler, PlainHandler};
use futures::StreamExt;
use itertools::Either;
use openssh::{OverSsh, OwningCommand, Session};
use serde::de::DeserializeOwned;
use tokio::{io::AsyncRead, process::Command, select};
use tokio_util::codec::{BytesCodec, FramedRead, LinesCodec};
use tracing::debug;

use crate::host::EscalationStrategy;

fn escape_bash(input: &str, out: &mut String) {
	const TO_ESCAPE: &str = "$ !\"#&'()*,;<>?[\\]^`{|}";
	if input.chars().all(|c| !TO_ESCAPE.contains(c)) {
		out.push_str(input);
		return;
	}
	out.push('\'');
	for (i, v) in input.split('\'').enumerate() {
		if i != 0 {
			out.push_str("'\"'\"'");
		}
		out.push_str(v);
	}
	out.push('\'');
}
fn ostoutf8(os: impl AsRef<OsStr>) -> String {
	os.as_ref().to_str().expect("non-utf8 data").to_owned()
}

#[derive(Clone, Debug)]
pub struct MyCommand {
	command: String,
	args: Vec<String>,
	env: Vec<(String, String)>,
	ssh_session: Option<Arc<Session>>,
	escalation: EscalationStrategy,
	escalate: bool,
}
impl MyCommand {
	pub fn new_on(
		escalation: EscalationStrategy,
		cmd: impl AsRef<OsStr>,
		session: Arc<Session>,
	) -> Self {
		assert!(!cmd.as_ref().is_empty());
		Self {
			command: ostoutf8(cmd),
			args: vec![],
			env: vec![],
			ssh_session: Some(session),
			escalation,
			escalate: false,
		}
	}
	pub fn new(escalation: EscalationStrategy, cmd: impl AsRef<OsStr>) -> Self {
		assert!(!cmd.as_ref().is_empty());
		Self {
			command: ostoutf8(cmd),
			args: vec![],
			env: vec![],
			ssh_session: None,
			escalation,
			escalate: false,
		}
	}
	fn new_here(&self, cmd: impl AsRef<OsStr>) -> Self {
		match self.ssh_session.clone() {
			Some(ssh_session) => Self::new_on(self.escalation, cmd, ssh_session),
			_ => Self::new(self.escalation, cmd),
		}
	}

	fn into_args(self) -> Vec<String> {
		let mut out = Vec::new();
		if !self.env.is_empty() {
			out.push("env".to_owned());
			for (k, v) in self.env {
				assert!(!k.contains('='));
				out.push(format!("{k}={v}"));
			}
		}
		out.push(self.command);
		out.extend(self.args);
		out
	}

	/// Translates environment variables into env command execution.
	/// Required for ssh, as ssh don't allow to send environment variables (at least by default).
	///
	/// FIXME: Insecure, as arguments might be seen by other users on the same machine.
	/// Figure out some way to transfer environment using stdio?
	fn translate_env_into_env(self) -> Self {
		if self.env.is_empty() {
			return self;
		}
		let mut out = self.new_here("env");
		for (k, v) in self.env {
			assert!(!k.contains('='));
			out.arg(format!("{k}={v}"));
		}
		out.arg(self.command);
		out.args(self.args);

		out
	}
	fn into_string(self) -> String {
		let mut out = String::new();
		if !self.env.is_empty() {
			out.push_str("env");
			for (k, v) in self.env {
				out.push(' ');
				assert!(!k.contains('='));
				escape_bash(&k, &mut out);
				out.push('=');
				escape_bash(&v, &mut out);
			}
		}
		if !out.is_empty() {
			out.push(' ');
		}
		escape_bash(&self.command, &mut out);
		for arg in self.args {
			out.push(' ');
			escape_bash(&arg, &mut out);
		}
		out
	}
	fn into_command_unchecked_local(self) -> Command {
		let mut out = Command::new(self.command);
		out.args(self.args);
		for (k, v) in self.env {
			out.env(k, v);
		}
		out
	}
	fn into_command(self) -> Result<Either<Command, openssh::OwningCommand<Arc<Session>>>> {
		Ok(match self.ssh_session.clone() {
			Some(session) => {
				let cmd = self.translate_env_into_env().into_command_unchecked_local();
				Either::Right(
					cmd.over_ssh(session)
						.map_err(|e| anyhow!("ssh error: {e}"))?,
				)
			}
			_ => {
				let cmd = self.into_command_unchecked_local();
				Either::Left(cmd)
			}
		})
	}
	pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
		let arg = arg.as_ref();
		self.args.push(ostoutf8(arg));
		self
	}
	pub fn eqarg(&mut self, arg: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
		let arg = arg.as_ref();
		let value = value.as_ref();
		let arg = ostoutf8(arg);
		let value = ostoutf8(value);
		self.arg(format!("{arg}={value}"));
		self
	}
	pub fn comparg(&mut self, arg: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
		self.arg(arg);
		self.arg(value);
		self
	}
	pub fn env(&mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
		self.env
			.push((name.as_ref().to_owned(), value.as_ref().to_owned()));
		self
	}
	pub fn args<V: AsRef<OsStr>>(&mut self, args: impl IntoIterator<Item = V>) -> &mut Self {
		for arg in args.into_iter() {
			let arg = arg.as_ref();
			self.args.push(ostoutf8(arg));
		}
		self
	}
	pub fn sudo(mut self) -> Self {
		self.escalate = true;
		self
	}
	fn wrap_sudo_if_needed(self) -> Self {
		if !self.escalate {
			return self;
		}
		match self.escalation {
			EscalationStrategy::Su => {
				let mut out = self.new_here("su");
				out.arg("-c").arg(self.into_string());
				out
			}
			EscalationStrategy::Sudo => {
				let mut out = self.new_here("sudo");
				out.args(self.into_args());
				out
			}
			EscalationStrategy::Run0 => {
				// run0 wants interactive authentication by default.
				let mut run0 = self.new_here("run0");
				let mut out = self.new_here("script");

				// Red backgrounds messes with fleet formatting
				run0.arg("--background=");
				run0.args(self.into_args());

				out.arg("-q");
				out.arg("/dev/null");
				out.arg("-c");
				out.arg(run0.into_string());
				dbg!(&out);
				out
			}
		}
	}

	pub async fn run(self) -> Result<()> {
		let str = self.clone().into_string();
		let cmd = self.wrap_sudo_if_needed().into_command()?;
		match cmd {
			Either::Left(cmd) => run_nix_inner(str, cmd, &mut PlainHandler).await?,
			Either::Right(cmd) => run_nix_inner_ssh(str, cmd, &mut PlainHandler).await?,
		};
		Ok(())
	}
	pub async fn run_string(self) -> Result<String> {
		let bytes = self.run_bytes().await?;
		Ok(String::from_utf8(bytes)?)
	}
	pub async fn run_value<T: DeserializeOwned>(self) -> Result<T> {
		let v = self.run_string().await?;
		Ok(serde_json::from_str(&v)?)
	}
	pub async fn run_bytes(self) -> Result<Vec<u8>> {
		let str = self.clone().into_string();
		let cmd = self.wrap_sudo_if_needed().into_command()?;
		let v = match cmd {
			Either::Left(cmd) => run_nix_inner_stdout(str, cmd, &mut PlainHandler).await?,
			Either::Right(cmd) => run_nix_inner_stdout_ssh(str, cmd, &mut PlainHandler).await?,
		};
		Ok(v)
	}

	pub async fn run_nix_string(mut self) -> Result<String> {
		let str = self.clone().into_string();
		self.arg("--log-format").arg("internal-json");
		let cmd = self.wrap_sudo_if_needed().into_command()?;
		let bytes = match cmd {
			Either::Left(cmd) => run_nix_inner_stdout(str, cmd, &mut NixHandler::default()).await?,
			Either::Right(cmd) => {
				run_nix_inner_stdout_ssh(str, cmd, &mut NixHandler::default()).await?
			}
		};
		Ok(String::from_utf8(bytes)?)
	}
	pub async fn run_nix(mut self) -> Result<()> {
		let str = self.clone().into_string();
		self.arg("--log-format").arg("internal-json");
		let cmd = self.wrap_sudo_if_needed().into_command()?;
		match cmd {
			Either::Left(mut cmd) => {
				cmd.stdout(Stdio::inherit());
				run_nix_inner(str, cmd, &mut NixHandler::default()).await
			}
			Either::Right(mut cmd) => {
				cmd.stdout(openssh::Stdio::inherit());
				run_nix_inner_ssh(str, cmd, &mut NixHandler::default()).await
			}
		}
	}
}

struct EmptyAsyncRead;
impl AsyncRead for EmptyAsyncRead {
	fn poll_read(
		self: std::pin::Pin<&mut Self>,
		_cx: &mut std::task::Context<'_>,
		_buf: &mut tokio::io::ReadBuf<'_>,
	) -> Poll<std::io::Result<()>> {
		Poll::Pending
	}
}

async fn run_nix_inner_stdout(
	str: String,
	cmd: Command,
	handler: &mut dyn Handler,
) -> Result<Vec<u8>> {
	Ok(run_nix_inner_raw(str, cmd, true, handler, None)
		.await?
		.expect("has out"))
}
async fn run_nix_inner(str: String, cmd: Command, handler: &mut dyn Handler) -> Result<()> {
	let v = run_nix_inner_raw(str, cmd, false, handler, None).await?;
	assert!(v.is_none());
	Ok(())
}
async fn run_nix_inner_stdout_ssh(
	str: String,
	cmd: OwningCommand<Arc<Session>>,
	handler: &mut dyn Handler,
) -> Result<Vec<u8>> {
	Ok(run_nix_inner_raw_ssh(str, cmd, true, handler, None)
		.await?
		.expect("has out"))
}
async fn run_nix_inner_ssh(
	str: String,
	cmd: OwningCommand<Arc<Session>>,
	handler: &mut dyn Handler,
) -> Result<()> {
	let v = run_nix_inner_raw_ssh(str, cmd, false, handler, None).await?;
	assert!(v.is_none());
	Ok(())
}

async fn run_nix_inner_raw(
	str: String,
	mut cmd: Command,
	want_stdout: bool,
	err_handler: &mut dyn Handler,
	mut out_handler: Option<&mut dyn Handler>,
) -> Result<Option<Vec<u8>>> {
	cmd.stderr(Stdio::piped());
	cmd.stdout(Stdio::piped());
	debug!("running command {str:?} on local");
	let mut child = cmd.spawn()?;
	let mut stderr = child.stderr.take().unwrap();
	let stdout = child.stdout.take().unwrap();
	let mut err = FramedRead::new(&mut stderr, LinesCodec::new());
	let mut out: Option<Box<dyn AsyncRead + Unpin>> = Some(Box::new(stdout));
	let mut ob = want_stdout
		.then(|| out.take().unwrap())
		.unwrap_or_else(|| Box::new(EmptyAsyncRead));
	let mut ol = (!want_stdout)
		.then(|| out.take().unwrap())
		.unwrap_or_else(|| Box::new(EmptyAsyncRead));
	let mut ob = FramedRead::new(&mut ob, BytesCodec::new());
	let mut ol = FramedRead::new(&mut ol, LinesCodec::new());

	// while let Some(line) = read.next().await? {}

	let mut out_buf = if want_stdout { Some(vec![]) } else { None };
	loop {
		select! {
			e = err.next() => {
				if let Some(e) = e {
					let e = e?;
					err_handler.handle_line(&e);
				}
			},
			o = ob.next() => {
				if let Some(o) = o {
					out_buf.as_mut().expect("stdout == wants_stdout").extend_from_slice(&o?);
				}
			},
			o = ol.next() => {
				if let Some(o) = o {
					let o = o?;
					if let Some(out) = out_handler.as_mut() {
						out.handle_line(&o)
					} else {
						err_handler.handle_line(&o)
					}
					// out_handler.handle_info(&o);
				}
			},
			code = child.wait() => {
				let code = code?;
				if !code.success() {
					anyhow::bail!("command '{str}' failed with status {}", code);
				}
				break;
			}
		}
	}

	Ok(out_buf)
}
async fn run_nix_inner_raw_ssh(
	str: String,
	mut cmd: OwningCommand<Arc<Session>>,
	want_stdout: bool,
	err_handler: &mut dyn Handler,
	mut out_handler: Option<&mut dyn Handler>,
) -> Result<Option<Vec<u8>>> {
	debug!("running command {str:?} over ssh");
	cmd.stderr(openssh::Stdio::piped());
	cmd.stdout(openssh::Stdio::piped());
	let mut child = cmd.spawn().await?;
	let mut stderr = child.stderr().take().unwrap();
	let stdout = child.stdout().take().unwrap();
	let mut err = FramedRead::new(&mut stderr, LinesCodec::new());
	let mut out: Option<Box<dyn AsyncRead + Unpin>> = Some(Box::new(stdout));
	let mut ob = want_stdout
		.then(|| out.take().unwrap())
		.unwrap_or_else(|| Box::new(EmptyAsyncRead));
	let mut ol = (!want_stdout)
		.then(|| out.take().unwrap())
		.unwrap_or_else(|| Box::new(EmptyAsyncRead));
	let mut ob = FramedRead::new(&mut ob, BytesCodec::new());
	let mut ol = FramedRead::new(&mut ol, LinesCodec::new());

	// while let Some(line) = read.next().await? {}

	let mut out_buf = if want_stdout { Some(vec![]) } else { None };

	let mut wait_future = pin::pin!(child.wait());
	loop {
		select! {
			e = err.next() => {
				if let Some(e) = e {
					let e = e?;
					err_handler.handle_line(&e);
				}
			},
			o = ob.next() => {
				if let Some(o) = o {
					out_buf.as_mut().expect("stdout == wants_stdout").extend_from_slice(&o?);
				}
			},
			o = ol.next() => {
				if let Some(o) = o {
					let o = o?;
					if let Some(out) = out_handler.as_mut() {
						out.handle_line(&o)
					} else {
						err_handler.handle_line(&o)
					}
					// out_handler.handle_info(&o);
				}
			},
			code = &mut wait_future => {
				let code = code?;
				if !code.success() {
					anyhow::bail!("command '{str}' failed with status {}", code);
				}
				break;
			}
		}
	}

	Ok(out_buf)
}
