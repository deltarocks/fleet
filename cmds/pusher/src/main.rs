use std::process::Stdio;
use std::{io, pin::pin};

use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::{Message, WebSocket};
use axum::response::Response;
use axum::routing::{any, get};
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use futures_util::{SinkExt, StreamExt as _};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_util::io::{CopyToBytes, SinkWriter, StreamReader};

async fn push(bearer: TypedHeader<Authorization<Bearer>>, req: WebSocketUpgrade) -> Response {
	if bearer.token() != "test-token" {
		todo!()
		// return Response::builder().status(403).body(()).expect("build");
	}
	req.on_upgrade(serve_nix)
}
async fn list_generations(bearer: TypedHeader<Authorization<Bearer>>, machine: String) -> Response {
	if bearer.token() != "test-token" {
		todo!()
		// return Response::builder().status(403).body(()).expect("build");
	}
	todo!()
}

async fn serve_nix(ws: WebSocket) {
	let child = Command::new("nix-store")
		.arg("--serve")
		.arg("--write")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.spawn()
		.unwrap();

	let (tx, rx) = ws.split();

	let mut read = pin!(StreamReader::new(rx.filter_map(|msg| async move {
		match msg {
			Ok(Message::Binary(b)) => Some(Ok(b)),
			Ok(Message::Ping(_)) => None,
			Ok(Message::Close(_)) => None,
			Ok(_) => Some(Err(io::Error::other("unexpected frame"))),
			Err(e) => Some(Err(io::Error::other(e))),
		}
	})));
	let mut write = pin!(SinkWriter::new(CopyToBytes::new(
		tx.with(|data: Bytes| async move { Ok::<_, axum::Error>(Message::Binary(data)) })
			.sink_map_err(|_| io::Error::other("idk")),
	)));
	let _ = tokio::io::copy(&mut read, &mut child.stdin.expect("stdin")).await;
	let _ = tokio::io::copy(&mut child.stdout.expect("stdout"), &mut write).await;
}

#[tokio::main]
async fn main() -> Result<()> {
	let router = Router::new()
		.route("/push", any(push))
		.route("/generations/{machine}", get(list_generations));
	axum::serve(TcpListener::bind("localhost:8111").await?, router).await?;
	Ok(())
}
