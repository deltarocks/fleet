use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc};

use better_command::NixHandler;
use serde::{Serialize, de::DeserializeOwned};

use crate::{Error, NixBuildBatch, NixSession, Result, macros::NixExprBuilder, nix_go};

#[derive(Clone)]
pub enum Index {
	Var(String),
	String(String),
	#[allow(dead_code)]
	Apply(String),
	#[allow(dead_code)]
	Expr(NixExprBuilder),
	ExprApply(NixExprBuilder),
	Pipe(NixExprBuilder),
	Merge(NixExprBuilder),
}
impl Index {
	pub fn var(v: impl AsRef<str>) -> Self {
		let v = v.as_ref();
		assert!(
			!(v.contains('.') | v.contains(' ')),
			"bad variable name: {v}"
		);
		Self::Var(v.to_owned())
	}
	pub fn attr(v: impl AsRef<str>) -> Self {
		Self::String(v.as_ref().to_owned())
	}
	#[allow(dead_code)]
	pub fn apply(v: impl Serialize) -> Self {
		let serialized = nixlike::serialize(v).expect("invalid value for apply");
		Self::Apply(serialized.trim_end().to_owned())
	}
}
impl fmt::Display for Index {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Index::Var(v) => {
				write!(f, "{v}")
			}
			Index::String(k) => {
				let v = nixlike::format_identifier(k.as_str());
				write!(f, ".{v}")
			}
			Index::Apply(_) => {
				write!(f, "<apply>(...)")
			}
			Index::Expr(e) => {
				write!(f, "[{}]", e.out)
			}
			Index::ExprApply(_) => {
				write!(f, "<apply>(...)")
			}
			Index::Pipe(e) => {
				write!(f, "<map>({})", e.out)
			}
			Index::Merge(e) => {
				write!(f, "//({})", e.out)
			}
		}
	}
}
impl fmt::Debug for Index {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{self}")
	}
}
struct PathDisplay<'i>(&'i [Index]);
impl fmt::Display for PathDisplay<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if !matches!(self.0.first(), Some(Index::Var(_))) {
			write!(f, "<unknown>")?;
		}
		for i in self.0 {
			write!(f, "{i}")?;
		}
		Ok(())
	}
}
struct ValueInner {
	full_path: Vec<Index>,
	session: NixSession,
	value: u32,
}
#[derive(Clone)]
pub struct Value(Arc<ValueInner>);
impl Value {
	async fn new(session: NixSession, query: &str) -> Result<Self> {
		let vid = session.0.lock().await.execute_assign(query).await?;
		Ok(Self(Arc::new(ValueInner {
			full_path: vec![],
			session,
			value: vid,
		})))
	}
	/// Get a top-level binding.
	///
	/// In flake repl session, every output is exposed as top-level binding.
	pub async fn binding(session: NixSession, query: &str) -> Result<Self> {
		// TODO: Verify that query is a valid variable name
		let vid = session.0.lock().await.execute_assign(query).await?;
		Ok(Self(Arc::new(ValueInner {
			full_path: vec![Index::Var(query.to_owned())],
			session,
			value: vid,
		})))
	}
	pub async fn select(&self, name: impl IntoIterator<Item = Index>) -> Result<Self> {
		let mut used_fields = Vec::new();
		let name = name.into_iter();

		let mut full_path = self.0.full_path.clone();
		let mut query = self.sess_field_name();
		for v in name {
			full_path.push(v.clone());
			match v {
				Index::Var(_) => panic!("var item may only be first"),
				Index::String(s) => {
					let escaped =
						nixlike::serialize(s).expect("strings are always serialized successfully");
					query.push('.');
					query.push_str(escaped.trim());
				}
				Index::Apply(a) => {
					// In cases like `a {}.b` first `{}.b` will be evaluated, so `a {}` should be encased in `()`
					query = format!("({query} {a})");
				}
				Index::Expr(e) => {
					let index = Value::new(self.0.session.clone(), &e.out).await?;
					used_fields.push(index.clone());
					query.push('.');
					let index = format!("${{sess_field_{}}}", index.0.value);
					query.push_str(&index);
				}
				Index::ExprApply(e) => {
					let index = Value::new(self.0.session.clone(), &e.out).await?;
					used_fields.push(index.clone());
					query.push(' ');
					let index = format!("sess_field_{}", index.0.value);
					query.push_str(&index);
					query = format!("({query})");
				}
				Index::Pipe(v) => {
					let index = Value::new(self.0.session.clone(), &v.out).await?;
					used_fields.push(index.clone());
					let index = format!("sess_field_{}", index.0.value);
					query = format!("({index} {query})");
				}
				Index::Merge(v) => {
					let index = Value::new(self.0.session.clone(), &v.out).await?;
					used_fields.push(index.clone());
					let index = format!("sess_field_{}", index.0.value);
					query = format!("({query} // {index})");
				}
			}
		}

		let vid = self
			.0
			.session
			.0
			.lock()
			.await
			.execute_assign(&query)
			.await
			.map_err(|e| e.context(self.attribute()))?;
		Ok(Self(Arc::new(ValueInner {
			full_path,
			session: self.0.session.clone(),
			value: vid,
		})))
	}
	pub async fn as_json<V: DeserializeOwned>(&self) -> Result<V> {
		let query = self.sess_field_name();
		self.0
			.session
			.0
			.lock()
			.await
			.execute_expression_to_json(&query)
			.await
			.map_err(|e| e.context(self.attribute()))
	}
	#[allow(dead_code)]
	pub async fn has_field(&self, name: &str) -> Result<bool> {
		let key = nixlike::escape_string(name);
		let query = format!("{} ? {key}", self.sess_field_name());
		self.0
			.session
			.0
			.lock()
			.await
			.execute_expression_to_json(&query)
			.await
			.map_err(|e| e.context(self.attribute()))
	}
	pub async fn list_fields(&self) -> Result<Vec<String>> {
		let query = format!("builtins.attrNames {}", self.sess_field_name());
		self.0
			.session
			.0
			.lock()
			.await
			.execute_expression_to_json(&query)
			.await
			.map_err(|e| e.context(self.attribute()))
	}
	pub async fn type_of(&self) -> Result<String> {
		let query = format!("builtins.typeOf {}", self.sess_field_name());
		self.0
			.session
			.0
			.lock()
			.await
			.execute_expression_to_json(&query)
			.await
			.map_err(|e| e.context(self.attribute()))
	}
	#[allow(dead_code)]
	pub async fn import(&self) -> Result<Self> {
		let import = Self::new(self.0.session.clone(), "import").await?;
		Ok(nix_go!(self | import))
	}
	fn sess_field_name(&self) -> String {
		format!("sess_field_{}", self.0.value)
	}
	pub async fn build_maybe_batch(
		&self,
		batch: Option<NixBuildBatch>,
	) -> Result<HashMap<String, PathBuf>> {
		if let Some(batch) = batch {
			batch.submit(self.clone()).await
		} else {
			self.build().await
		}
	}
	pub async fn build(&self) -> Result<HashMap<String, PathBuf>> {
		let query = format!(":b {}", self.sess_field_name());
		let vid = self
			.0
			.session
			.0
			.lock()
			.await
			.execute_expression_raw(&query, &mut NixHandler::default())
			.await?;
		if vid.is_empty() {
			return Err(Error::BuildFailed {
				attribute: self.attribute(),
				error: "build produced no output".to_owned(),
			});
		}
		let Some(vid) = vid.strip_prefix("This derivation produced the following outputs:\n")
		else {
			return Err(Error::BuildFailed {
				attribute: self.attribute(),
				error: format!("failed to parse output: {vid}"),
			});
		};
		let outputs = vid
			.split('\n')
			.filter(|v| !v.is_empty())
			.map(|v| v.split_once(" -> ").expect("unexpected build output"))
			.map(|(a, b)| (a.trim_start().to_owned(), PathBuf::from(b)))
			.collect();
		Ok(outputs)
	}
	/// Weakly convert string-like types (derivation/path/string) to string
	pub async fn to_string_weak(&self) -> Result<String> {
		let query = format!("\"${{{}}}\"", self.sess_field_name());
		let vid: String = self
			.0
			.session
			.0
			.lock()
			.await
			.execute_expression_to_json(&query)
			.await?;
		Ok(vid)
	}

	fn attribute(&self) -> String {
		PathDisplay(&self.0.full_path).to_string()
	}

	pub(crate) fn session(&self) -> NixSession {
		self.0.session.clone()
	}

	pub(crate) fn session_field_id(&self) -> u32 {
		self.0.value
	}
}
impl Drop for ValueInner {
	fn drop(&mut self) {
		if let Ok(mut lock) = self.session.0.try_lock() {
			lock.free_list.push(self.value)
		}
		// Leaked
	}
}
