use bifrostlink::error::{ErrorT, ListenerForYourRequestHasBeenDeadError, ResponseError};
use bifrostlink::notification;
use bifrostlink::packet::OpaquePacketWrapper;
use bifrostlink::AddressT;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod editor;
pub mod gateway;
pub mod iroh_tunnel;
pub mod port;

#[derive(Clone, Serialize, Hash, Eq, Debug, PartialEq, Deserialize)]
pub enum Address {
	User,
	Agent,
	AgentPrivileged,
	Plugin(u16),
	Ephemeral(uuid::Uuid),
}
impl AddressT for Address {}

pub mod plugin;

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("listener is dead")]
	ListenerDead,
	#[error("response: {0}")]
	Response(String),
}

impl From<ListenerForYourRequestHasBeenDeadError> for Error {
	fn from(_value: ListenerForYourRequestHasBeenDeadError) -> Self {
		Self::ListenerDead
	}
}
impl From<serde_json::Error> for Error {
	fn from(e: serde_json::Error) -> Self {
		Self::Response(format!("{e}"))
	}
}
impl From<Error> for ResponseError {
	fn from(val: Error) -> Self {
		ResponseError(val.to_string())
	}
}
impl From<ResponseError> for Error {
	fn from(value: ResponseError) -> Self {
		Self::Response(value.0)
	}
}
impl ErrorT for Error {}

#[derive(Serialize, Deserialize, Debug)]
pub struct TestNotification {
	pub value: u32,
}
notification!((0x0100) TestNotification);

pub struct BifConfig;
impl bifrostlink::Config for BifConfig {
	type Address = Address;

	type Error = Error;

	type EncodedData = Vec<u8>;

	fn decode_headers(
		data_with_headers: Bytes,
	) -> Result<(OpaquePacketWrapper<Self::Address>, Self::EncodedData), Self::Error> {
		let (header, data): (OpaquePacketWrapper<Self::Address>, Vec<u8>) =
			serde_json::from_slice(&data_with_headers)?;
		Ok((header, data))
	}

	fn decode_data<T: DeserializeOwned>(data: Self::EncodedData) -> Result<T, Self::Error> {
		let v: T = serde_json::from_slice(&data)?;
		Ok(v)
	}

	fn encode_data<T: Serialize>(headers: OpaquePacketWrapper<Self::Address>, data: T) -> Bytes {
		let data = serde_json::to_vec(&data).expect("serialization shouldn't fail");
		let o = serde_json::to_vec(&(headers, data)).expect("serialization shouldn't fail");
		o.into()
	}
}
