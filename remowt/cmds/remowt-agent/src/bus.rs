use std::process::Stdio;

use anyhow::Context as _;
use futures::StreamExt as _;
use tokio::process::{Child, Command};
use tokio_util::codec::{FramedRead, LinesCodec};
use zbus::Connection;

pub struct PrivateBus {
	pub address: String,
	pub conn: Connection,
	_child: Child,
}

pub async fn spawn() -> anyhow::Result<PrivateBus> {
	let mut child = Command::new("dbus-daemon")
		.args(["--session", "--nofork", "--print-address"])
		.stdout(Stdio::piped())
		.kill_on_drop(true)
		.spawn()
		.context("spawning dbus-daemon for the private bus")?;

	let stdout = child.stdout.take().expect("piped");
	let address = FramedRead::new(stdout, LinesCodec::new())
		.next()
		.await
		.context("dbus-daemon exited before printing its address")?
		.context("reading dbus-daemon address")?;

	let conn = zbus::connection::Builder::address(address.as_str())?
		.build()
		.await
		.context("connecting to the private bus")?;

	Ok(PrivateBus {
		address,
		conn,
		_child: child,
	})
}
