use std::{
	ffi::OsString,
	sync::{Arc, OnceLock},
};

use r2d2::Pool;

use crate::{Error, NixSession, Result, session::NixSessionInner};

pub struct NixSessionPool(Pool<NixSessionPoolInner>);
impl NixSessionPool {
	pub async fn new(
		flake: OsString,
		nix_args: Vec<OsString>,
		nix_system: String,
		fail_fast: bool,
	) -> Result<Self> {
		let inner = tokio::task::block_in_place(|| {
			r2d2::Builder::<NixSessionPoolInner>::new()
				.min_idle(Some(0))
				.build(NixSessionPoolInner {
					flake,
					nix_args,
					nix_system,
					fail_fast,
				})
		})?;
		Ok(Self(inner))
	}
	pub async fn get(&self) -> Result<NixSession> {
		let v = tokio::task::block_in_place(|| self.0.get())?;
		Ok(NixSession(Arc::new(tokio::sync::Mutex::new(v))))
	}
}

pub(crate) struct NixSessionPoolInner {
	flake: OsString,
	nix_args: Vec<OsString>,
	fail_fast: bool,
	pub(crate) nix_system: String,
}

impl r2d2::ManageConnection for NixSessionPoolInner {
	type Connection = NixSessionInner;
	type Error = Error;
	fn connect(&self) -> std::result::Result<Self::Connection, Self::Error> {
		let _v = TOKIO_RUNTIME
			.get()
			.expect("missed tokio runtime init!")
			.enter();
		futures::executor::block_on(NixSessionInner::new(
			self.flake.as_os_str(),
			self.nix_args.iter().map(OsString::as_os_str),
			self.nix_system.clone(),
			self.fail_fast,
		))
	}

	fn is_valid(&self, conn: &mut Self::Connection) -> std::result::Result<(), Self::Error> {
		let _v = TOKIO_RUNTIME
			.get()
			.expect("missed tokio runtime init!")
			.enter();
		let res = futures::executor::block_on(conn.execute_expression_number("2 + 2"))?;
		if res != 4 {
			// just in case, should fail much earlier
			return Err(Error::SessionInit("misbehaving session"));
		};
		Ok(())
	}

	fn has_broken(&self, _conn: &mut Self::Connection) -> bool {
		false
	}
}
pub static TOKIO_RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();
