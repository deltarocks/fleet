use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::trace;

use crate::{Error, Prompter, Result, Source};

#[derive(Clone)]
pub struct RofiPrompter;

fn fixup_prompt(prompt: &str) -> &str {
	// Rofi always appends such suffix
	prompt.strip_suffix(": ").unwrap_or(prompt)
}

fn rofi_command() -> Command {
	Command::new(option_env!("ROFI").unwrap_or("rofi"))
}

impl Prompter for RofiPrompter {
	async fn prompt_enum(
		&self,
		prompt: &str,
		description: &str,
		variants: &[&str],
		source: &[Source],
	) -> Result<u32> {
		trace!("rofi radio");
		let mut cmd = rofi_command();
		let mesg = if source.is_empty() {
			description.to_owned()
		} else {
			let mut out = format!("{description}\n\n<b>Requested on ",);
			for (i, s) in source.iter().enumerate() {
				if i != 0 {
					out.push_str(" -> ");
				}
				out.push_str(&s.to_string());
			}
			out.push_str("</b>");
			out
		};
		cmd.args([
			"-dmenu",
			"-mesg",
			&mesg,
			"-sync",
			"-no-custom",
			"-p",
			fixup_prompt(prompt),
			"-format",
			"i",
			"-markup-rows",
		]);
		cmd.stdin(Stdio::piped());
		cmd.stdout(Stdio::piped());
		cmd.kill_on_drop(true);
		let mut child = cmd
			.spawn()
			.map_err(|e| Error::InputError(format!("failed to spawn rofi: {e}")))?;

		let mut stdin = child.stdin.take().expect("stdin is piped");
		for var in variants {
			stdin
				.write_all(var.replace('\n', " ").as_bytes())
				.await
				.map_err(|e| Error::InputError(format!("failed to write rofi variants: {e}")))?;
			stdin
				.write_all(b"\n")
				.await
				.map_err(|e| Error::InputError(format!("failed to write rofi variants: {e}")))?;
		}
		// write_all already flushes, just to be sure.
		let _ = stdin.flush().await;
		drop(stdin);

		let out = child
			.wait_with_output()
			.await
			.map_err(|e| Error::InputError(format!("failed to wait for rofi: {e}")))?;
		match out.status.code() {
			Some(0) => {}
			Some(1) => return Err(Error::Cancel),
			other => {
				return Err(Error::InputError(format!(
					"rofi exited with status {other:?}"
				)));
			}
		}
		let stdout = out
			.stdout
			.strip_suffix(b"\n")
			.unwrap_or(&out.stdout)
			.to_owned();

		let id: u32 = String::from_utf8(stdout)
			.map_err(|e| Error::InputError(format!("rofi produced invalid output: {e}")))?
			.parse()
			.map_err(|e| Error::InputError(format!("rofi produced invalid output: {e}")))?;
		if id as usize >= variants.len() {
			return Err(Error::InputError("invalid rofi response".to_owned()));
		}

		Ok(id)
	}

	async fn prompt_text(
		&self,
		echo: bool,
		prompt: &str,
		description: &str,
		source: &[Source],
	) -> Result<String> {
		trace!("rofi text");
		let mut cmd = rofi_command();
		let mesg = if source.is_empty() {
			description.to_owned()
		} else {
			let mut out = format!("{description}\n\n<b>Requested on ",);
			for (i, s) in source.iter().enumerate() {
				if i != 0 {
					out.push_str(" -> ");
				}
				out.push_str(&s.to_string());
			}
			out.push_str("</b>");
			out
		};
		cmd.args(["-dmenu", "-mesg", &mesg, "-p", fixup_prompt(prompt)]);
		if !echo {
			cmd.arg("-password");
		}
		cmd.stdin(Stdio::null());
		cmd.stdout(Stdio::piped());
		cmd.kill_on_drop(true);
		let child = cmd
			.spawn()
			.map_err(|e| Error::InputError(format!("failed to spawn rofi: {e}")))?;

		let out = child
			.wait_with_output()
			.await
			.map_err(|e| Error::InputError(format!("failed to wait for rofi: {e}")))?;
		match out.status.code() {
			Some(0) => {}
			Some(1) => return Err(Error::Cancel),
			other => {
				return Err(Error::InputError(format!(
					"rofi exited with status {other:?}"
				)));
			}
		}
		let stdout = out
			.stdout
			.strip_suffix(b"\n")
			.unwrap_or(&out.stdout)
			.to_owned();

		Ok(String::from_utf8_lossy(&stdout).to_string())
	}

	async fn display_text(&self, error: bool, description: &str, source: &[Source]) -> Result<()> {
		trace!("rofi display");
		let mut cmd = rofi_command();
		let mut mesg = if source.is_empty() {
			description.to_owned()
		} else {
			let mut out = format!("{description}\n\n<b>Coming from ",);
			for s in source.iter() {
				out.push_str(&s.to_string());
			}
			out.push_str("</b>");
			out
		};
		if error {
			mesg.insert_str(0, "<span color=\"red\">");
			mesg.push_str("</span>");
		}
		cmd.args(["-e", &mesg, "-markup"]);
		cmd.stdin(Stdio::null());
		cmd.stdout(Stdio::null());
		cmd.kill_on_drop(true);
		let mut child = cmd
			.spawn()
			.map_err(|e| Error::InputError(format!("failed to spawn rofi: {e}")))?;

		child
			.wait()
			.await
			.map_err(|e| Error::InputError(format!("failed to wait for rofi: {e}")))?;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use crate::rofi::RofiPrompter;
	use crate::{PrependSourcePrompter, Prompter as _, Source};

	// #[tokio::test]
	#[tokio::test]
	#[ignore = "interactive"]
	async fn test() {
		let prompter = PrependSourcePrompter {
			prompter: RofiPrompter,
			description: "test".to_owned(),
			source: vec![Source(Cow::Borrowed("ssh"))],
		};
		prompter
			.prompt_radio("Enable", "Polkit needs access", &[])
			.await
			.expect("rofi");
		prompter
			.prompt_text(false, "Password", "Polkit needs access", &[])
			.await
			.expect("rofi");
		prompter
			.display_text(true, "Polkit needs access", &[])
			.await
			.expect("rofi");
	}
}
