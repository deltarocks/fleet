use std::io::ErrorKind;
use std::str::FromStr;
use std::sync::Mutex;

use bifrostlink::declarative::endpoints;
use bifrostlink::Config;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Default)]
pub struct Fs {
	tempdirs: Mutex<Vec<TempDir>>,
}

impl Fs {
	pub fn new() -> Self {
		Self::default()
	}
}

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
pub enum Error {
	#[error("file not found")]
	NotFound,
	#[error("file name/contents is not utf8")]
	InvalidUtf8,
	#[error("unknown fs error")]
	Unknown,
}

#[endpoints(ns = 1)]
impl Fs {
	#[endpoints(id = 1)]
	async fn read_file_tiny(&self, path: Utf8PathBuf) -> Result<Vec<u8>, Error> {
		match tokio::fs::read(path).await {
			Ok(v) => Ok(v),
			Err(e) if e.kind() == ErrorKind::NotFound => Err(Error::NotFound),
			_ => Err(Error::Unknown),
		}
	}
	#[endpoints(id = 2)]
	async fn file_exists(&self, path: Utf8PathBuf) -> bool {
		tokio::fs::try_exists(path).await.unwrap_or(false)
	}
	#[endpoints(id = 3)]
	async fn read_dir_raw(&self, path: Utf8PathBuf) -> Result<Vec<Utf8PathBuf>, Error> {
		let mut dir = match tokio::fs::read_dir(path).await {
			Ok(dir) => dir,
			Err(e) if e.kind() == ErrorKind::NotFound => return Err(Error::NotFound),
			Err(_) => return Err(Error::Unknown),
		};
		let mut out = Vec::new();
		while let Ok(Some(entry)) = dir.next_entry().await {
			let name = Utf8PathBuf::try_from(entry.file_name()).map_err(|_| Error::InvalidUtf8)?;
			out.push(name);
		}
		Ok(out)
	}
	#[endpoints(id = 4)]
	async fn mktemp_dir_raw(&self) -> Result<Utf8PathBuf, Error> {
		let dir = tempfile::Builder::new()
			.prefix("remowt.")
			.tempdir()
			.map_err(|_| Error::Unknown)?;
		let mut tempdirs = self.tempdirs.lock().expect("not poisoned");
		let path = Utf8PathBuf::try_from(dir.path().to_owned()).map_err(|_| Error::InvalidUtf8);
		tempdirs.push(dir);
		path
	}
	#[endpoints(id = 5)]
	async fn rm_file(&self, path: Utf8PathBuf) -> Result<(), Error> {
		match tokio::fs::remove_file(path).await {
			Ok(()) => Ok(()),
			Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
			Err(_) => Err(Error::Unknown),
		}
	}
}

impl<C: Config> FsClient<C> {
	pub async fn read_file_text(&self, path: impl Into<Utf8PathBuf>) -> Result<String, Error> {
		let v = self
			.read_file_tiny(path.into())
			.await
			.map_err(|_| Error::Unknown)?;
		let v = v?;
		String::from_utf8(v).map_err(|_| Error::InvalidUtf8)
	}
	pub async fn read_file_value<T: FromStr>(
		&self,
		path: impl Into<Utf8PathBuf>,
	) -> Result<Result<T, T::Err>, Error> {
		let text = self.read_file_text(path).await?;
		Ok(T::from_str(&text))
	}
	pub async fn mktemp_dir(&self) -> Result<Utf8PathBuf, Error> {
		self.mktemp_dir_raw().await.map_err(|_| Error::Unknown)?
	}
	pub async fn read_dir(&self, path: impl Into<Utf8PathBuf>) -> Result<Vec<Utf8PathBuf>, Error> {
		self.read_dir_raw(path.into())
			.await
			.map_err(|_| Error::Unknown)?
	}
}
