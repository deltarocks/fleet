use std::pin::pin;

use anyhow::{anyhow, bail, Result};
use camino::Utf8PathBuf;
use futures::StreamExt as _;
use remowt_endpoints::subprocess::{ProcId, SpawnSpec, StderrSpec, StdioSpec, SubprocessClient};
use remowt_link_shared::iroh_tunnel::TunnelAddr;
use remowt_link_shared::BifConfig;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt as _;
use tokio::select;
use tokio_util::codec::{BytesCodec, FramedRead, LinesCodec};
use tracing::{debug, info, warn};

use crate::forwarded::{RemowtListener, RemowtStream};
use crate::{drain_to_tracing, Remowt};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StdioMode {
	#[default]
	Null,
	Pipe,
	Inherit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StderrMode {
	#[default]
	Null,
	Pipe,
	Inherit,
	MergeWithStdout,
}

#[derive(Default)]
pub struct SpawnOptions {
	pub program: String,
	pub args: Vec<String>,
	pub env: Vec<(String, String)>,
	pub env_clear: bool,
	pub cwd: Option<Utf8PathBuf>,
	pub escalated: bool,
	pub stdin: StdioMode,
	pub stdout: StdioMode,
	pub stderr: StderrMode,
}

pub struct RemowtChild {
	pub stdin: Option<RemowtStream>,
	pub stdout: Option<RemowtStream>,
	pub stderr: Option<RemowtStream>,
	id: ProcId,
	client: SubprocessClient<BifConfig>,
}

impl RemowtChild {
	pub async fn wait(self) -> Result<Option<i32>> {
		let RemowtChild {
			stdin,
			stdout,
			stderr,
			id,
			client,
		} = self;
		drop(stdin);
		let drain_out = async move {
			if let Some(s) = stdout {
				let _ = drain_to_tracing(s, "<child stdout>".to_owned(), false).await;
			}
		};
		let drain_err = async move {
			if let Some(s) = stderr {
				let _ = drain_to_tracing(s, "<child stderr>".to_owned(), true).await;
			}
		};
		let wait = async move {
			client
				.wait(id)
				.await?
				.map_err(|e| anyhow!("agent wait failed: {e}"))
		};
		let (code, _, _) = tokio::join!(wait, drain_out, drain_err);
		code
	}

	pub async fn kill(&self, signal: i32) -> Result<()> {
		self.client
			.kill(self.id, signal)
			.await?
			.map_err(|e| anyhow!("agent kill failed: {e}"))
	}
}

fn needs_socket(m: StdioMode) -> bool {
	matches!(m, StdioMode::Pipe | StdioMode::Inherit)
}

fn stderr_needs_socket(m: StderrMode) -> bool {
	matches!(m, StderrMode::Pipe | StderrMode::Inherit)
}

impl Remowt {
	pub async fn spawn(&self, opts: SpawnOptions) -> Result<RemowtChild> {
		let SpawnOptions {
			program,
			args,
			env,
			env_clear,
			cwd,
			escalated,
			stdin,
			stdout,
			stderr,
		} = opts;

		if matches!(stderr, StderrMode::MergeWithStdout) && !needs_socket(stdout) {
			bail!("stderr=MergeWithStdout requires stdout=Pipe or Inherit");
		}

		let stdin_bound = if needs_socket(stdin) {
			Some(self.bind_fast_tunnel("proc-stdin", escalated).await?)
		} else {
			None
		};
		let stdout_bound = if needs_socket(stdout) {
			Some(self.bind_fast_tunnel("proc-stdout", escalated).await?)
		} else {
			None
		};
		let stderr_bound = if stderr_needs_socket(stderr) {
			Some(self.bind_fast_tunnel("proc-stderr", escalated).await?)
		} else {
			None
		};

		let stdin_spec = match &stdin_bound {
			Some((_, t)) => StdioSpec::Tunnel(t.clone()),
			None => StdioSpec::Null,
		};
		let stdout_spec = match &stdout_bound {
			Some((_, t)) => StdioSpec::Tunnel(t.clone()),
			None => StdioSpec::Null,
		};
		let stderr_spec = match (&stderr, &stderr_bound) {
			(StderrMode::Pipe | StderrMode::Inherit, Some((_, t))) => StderrSpec::Tunnel(t.clone()),
			(StderrMode::MergeWithStdout, _) => StderrSpec::MergeWithStdout,
			_ => StderrSpec::Null,
		};

		let client: SubprocessClient<BifConfig> = if escalated {
			// Boxed to break the async-fn type cycle
			Box::pin(self.run0_endpoints::<SubprocessClient<BifConfig>>()).await?
		} else {
			self.endpoints()
		};

		let spec = SpawnSpec {
			program: program.clone(),
			args,
			env,
			env_clear,
			cwd,
			stdin: stdin_spec,
			stdout: stdout_spec,
			stderr: stderr_spec,
		};
		let id = client
			.spawn(spec)
			.await?
			.map_err(|e| anyhow!("agent spawn failed: {e}"))?;

		let (stdin_res, stdout_res, stderr_res) = tokio::join!(
			accept(stdin_bound),
			accept(stdout_bound),
			accept(stderr_bound),
		);

		let stdin_stream = handle_stdin(stdin, stdin_res?, &program);
		let stdout_stream = handle_output(stdout, stdout_res?, &program);
		let stderr_stream = handle_output_err(stderr, stderr_res?, &program);

		Ok(RemowtChild {
			stdin: stdin_stream,
			stdout: stdout_stream,
			stderr: stderr_stream,
			id,
			client,
		})
	}

	pub fn cmd(&self, program: impl AsRef<str>) -> RemowtCommand {
		let program = program.as_ref().to_owned();
		RemowtCommand {
			program,
			args: vec![],
			env: vec![],
			remowt: self.clone(),
			escalated: false,
		}
	}
}

async fn accept(b: Option<(RemowtListener, TunnelAddr)>) -> Result<Option<RemowtStream>> {
	match b {
		Some((l, _)) => Ok(Some(l.accept().await?)),
		None => Ok(None),
	}
}

fn handle_stdin(mode: StdioMode, s: Option<RemowtStream>, program: &str) -> Option<RemowtStream> {
	match mode {
		StdioMode::Pipe => s,
		StdioMode::Inherit => {
			if let Some(s) = s {
				let program = program.to_owned();
				tokio::spawn(async move {
					let mut stdin = tokio::io::stdin();
					let mut s = s;
					if let Err(e) = tokio::io::copy(&mut stdin, &mut s).await {
						warn!(program, "stdin forward ended: {e}");
					}
					let _ = s.shutdown().await;
				});
			}
			None
		}
		StdioMode::Null => None,
	}
}

fn handle_output(mode: StdioMode, s: Option<RemowtStream>, program: &str) -> Option<RemowtStream> {
	match mode {
		StdioMode::Pipe => s,
		StdioMode::Inherit => {
			if let Some(s) = s {
				let program = program.to_owned();
				tokio::spawn(drain_to_tracing(s, program, false));
			}
			None
		}
		StdioMode::Null => None,
	}
}

fn handle_output_err(
	mode: StderrMode,
	s: Option<RemowtStream>,
	program: &str,
) -> Option<RemowtStream> {
	match mode {
		StderrMode::Pipe => s,
		StderrMode::Inherit => {
			if let Some(s) = s {
				let program = program.to_owned();
				tokio::spawn(drain_to_tracing(s, program, true));
			}
			None
		}
		StderrMode::MergeWithStdout | StderrMode::Null => None,
	}
}

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

#[derive(Clone)]
pub struct RemowtCommand {
	program: String,
	args: Vec<String>,
	env: Vec<(String, String)>,
	remowt: Remowt,
	escalated: bool,
}

impl RemowtCommand {
	pub fn arg(&mut self, arg: impl AsRef<str>) -> &mut Self {
		self.args.push(arg.as_ref().to_owned());
		self
	}
	pub fn args<V: AsRef<str>>(&mut self, args: impl IntoIterator<Item = V>) -> &mut Self {
		for arg in args {
			self.args.push(arg.as_ref().to_owned());
		}
		self
	}
	pub fn eqarg(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
		self.args
			.push(format!("{}={}", key.as_ref(), value.as_ref()));
		self
	}
	pub fn comparg(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
		self.args.push(key.as_ref().to_owned());
		self.args.push(value.as_ref().to_owned());
		self
	}
	pub fn env(&mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
		self.env
			.push((name.as_ref().to_owned(), value.as_ref().to_owned()));
		self
	}

	pub fn sudo(mut self) -> Self {
		self.escalated = true;
		self
	}

	/// Only for display.
	fn shell_line(&self) -> String {
		let mut out = String::new();
		if self.escalated {
			out.push_str("run0 ");
		}
		if !self.env.is_empty() {
			out.push_str("env");
			for (k, v) in &self.env {
				out.push(' ');
				assert!(!k.contains('='));
				escape_bash(k, &mut out);
				out.push('=');
				escape_bash(v, &mut out);
			}
			out.push(' ');
		}
		escape_bash(&self.program, &mut out);
		for arg in &self.args {
			out.push(' ');
			escape_bash(arg, &mut out);
		}
		out
	}

	fn into_spawn_options(self) -> (Remowt, SpawnOptions, String) {
		let line = self.shell_line();
		let opts = SpawnOptions {
			program: self.program,
			args: self.args,
			env: self.env,
			env_clear: false,
			cwd: None,
			escalated: self.escalated,
			stdin: StdioMode::Null,
			stdout: StdioMode::Pipe,
			stderr: StderrMode::Pipe,
		};
		(self.remowt, opts, line)
	}

	pub async fn run(self) -> Result<()> {
		run_inner(self, false).await.map(|_| ())
	}
	pub async fn run_string(self) -> Result<String> {
		let bytes = run_inner(self, true).await?.expect("want_stdout");
		Ok(String::from_utf8(bytes)?)
	}
	pub async fn run_value<T: DeserializeOwned>(self) -> Result<T> {
		let s = self.run_string().await?;
		Ok(serde_json::from_str(&s)?)
	}
}

async fn run_inner(cmd: RemowtCommand, want_stdout: bool) -> Result<Option<Vec<u8>>> {
	let (remowt, opts, line) = cmd.into_spawn_options();
	debug!("running command {line:?} over remowt");
	let program = opts.program.clone();
	let mut child = remowt.spawn(opts).await?;
	let stderr = child.stderr.take().expect("stderr=Pipe");
	let stdout = child.stdout.take().expect("stdout=Pipe");

	let mut err = FramedRead::new(stderr, LinesCodec::new());
	let (mut out_bytes, mut out_lines) = if want_stdout {
		(Some(FramedRead::new(stdout, BytesCodec::new())), None)
	} else {
		(None, Some(FramedRead::new(stdout, LinesCodec::new())))
	};

	let mut buf = if want_stdout { Some(Vec::new()) } else { None };

	let mut wait = pin!(child.wait());
	let exit = loop {
		select! {
			biased;

			Some(e) = err.next() => {
				let e = e?;
				warn!(program = %program, "{e}");
			}
			Some(o) = async { out_bytes.as_mut()?.next().await }, if want_stdout => {
				buf.as_mut().expect("want_stdout").extend_from_slice(&o?);
			}
			Some(o) = async { out_lines.as_mut()?.next().await }, if !want_stdout => {
				let o = o?;
				info!(program = %program, "{o}");
			}
			res = &mut wait => {
				break res?;
			}
		}
	};

	while let Some(e) = err.next().await {
		if let Ok(line) = e {
			warn!(program = %program, "{line}");
		}
	}
	if want_stdout {
		if let Some(out_bytes) = out_bytes.as_mut() {
			while let Some(o) = out_bytes.next().await {
				if let Ok(chunk) = o {
					buf.as_mut().expect("want_stdout").extend_from_slice(&chunk);
				}
			}
		}
	} else if let Some(out_lines) = out_lines.as_mut() {
		while let Some(o) = out_lines.next().await {
			if let Ok(line) = o {
				info!(program = %program, "{line}");
			}
		}
	}

	match exit {
		Some(0) => Ok(buf),
		Some(c) => bail!("command '{line}' failed with status {c}"),
		None => Err(anyhow!("command '{line}' ended without an exit status")),
	}
}
