//! This whole library should be replaced with either binding to nix libexpr,
//! or with tvix (once it is able to build NixOS).
//!
//! Current api is awful, little effort was put into this implementation.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::fmt;
use std::ptr::null_mut;
use std::sync::LazyLock;
use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use anyhow::Result;

use self::logging::nix_logging_cxx;
use self::nix_cxx::set_fetcher_setting;
use self::nix_raw::{
	alloc_value, c_context, c_context_create, err_code, err_info_msg, eval_state_build,
	eval_state_builder_new, expr_eval_from_string, fetchers_settings, fetchers_settings_free,
	fetchers_settings_new, flake_lock, flake_lock_flags, flake_lock_flags_free,
	flake_lock_flags_new, flake_reference_parse_flags, flake_reference_parse_flags_free,
	flake_reference_parse_flags_new, flake_reference_parse_flags_set_base_directory,
	flake_settings, flake_settings_free, flake_settings_new, init_bool, init_int, init_string,
	locked_flake_free, locked_flake_get_output_attrs, set_err_msg, setting_set, state_free,
	value_decref, value_force, value_incref,
};

mod value;
// Contains macros helpers
pub mod logging;
#[doc(hidden)]
pub mod macros;
pub mod util;

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
mod nix_raw {
	include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
#[cxx::bridge]
pub mod nix_cxx {
	unsafe extern "C++" {
		type nix_fetchers_settings;
		include!("nix-eval/src/lib.hh");

		unsafe fn set_fetcher_setting(
			settings: *mut nix_fetchers_settings,
			setting: *const c_char,
			value: *const c_char,
		);
	}
}

#[derive(Debug, PartialEq, Eq)]
pub enum NixType {
	Thunk,
	Int,
	Float,
	Bool,
	String,
	Path,
	Null,
	Attrs,
	List,
	Function,
	External,
}
impl NixType {
	fn from_int(c: c_uint) -> Self {
		match c {
			0 => Self::Thunk,
			1 => Self::Int,
			2 => Self::Float,
			3 => Self::Bool,
			4 => Self::String,
			5 => Self::Path,
			6 => Self::Null,
			7 => Self::Attrs,
			8 => Self::List,
			9 => Self::Function,
			10 => Self::External,
			_ => unreachable!("unknown nix type: {c}"),
		}
	}
}

#[derive(Debug)]
#[repr(i32)]
enum NixErrorKind {
	Unknown = 1,
	Overflow = 2,
	Key = 3,
	Generic = 4,
}
impl NixErrorKind {
	fn from_int(v: c_int) -> Option<Self> {
		Some(match v {
			0 => return None,
			-1 => Self::Unknown,
			-2 => Self::Overflow,
			-3 => Self::Key,
			-4 => Self::Generic,
			_ => {
				debug_assert!(false, "unexpected nix error kind: {v}");
				Self::Unknown
			}
		})
	}
}

pub fn gc_register_my_thread() {
	assert_eq!(unsafe { nix_raw::GC_thread_is_registered() }, 0);

	let mut sb = nix_raw::GC_stack_base {
		mem_base: null_mut(),
	};
	let r = unsafe { nix_raw::GC_get_stack_base(&mut sb) };
	if r as u32 != nix_raw::GC_SUCCESS {
		panic!("failed to get thread stack base");
	}
	unsafe { nix_raw::GC_register_my_thread(&sb) };
}
pub fn gc_unregister_my_thread() {
	assert_eq!(unsafe { nix_raw::GC_thread_is_registered() }, 1);

	unsafe { nix_raw::GC_unregister_my_thread() };
}

struct ThreadRegisterGuard {}
impl ThreadRegisterGuard {
	fn new() -> Self {
		gc_register_my_thread();
		Self {}
	}
}
impl Drop for ThreadRegisterGuard {
	fn drop(&mut self) {
		gc_unregister_my_thread();
	}
}

struct NixContext(*mut c_context);
impl NixContext {
	fn set_err(&mut self, err: NixErrorKind, msg: &CStr) {
		unsafe { set_err_msg(self.0, err as c_int, msg.as_ptr()) };
	}
	fn new() -> Self {
		let ctx = unsafe { c_context_create() };
		Self(ctx)
	}
	fn error_kind(&self) -> Option<NixErrorKind> {
		let code = unsafe { err_code(self.0) };
		NixErrorKind::from_int(code)
	}
	fn error<'t>(&self) -> Option<Cow<'t, str>> {
		if let NixErrorKind::Generic = self.error_kind()? {
			let mut err_out = String::new();
			unsafe {
				err_info_msg(
					null_mut(),
					self.0,
					Some(copy_nix_str),
					(&raw mut err_out).cast(),
				)
			};
			return Some(Cow::Owned(err_out));
		};

		// TODO: Can throw error (resulting in panic) if unable to retrieve error. Should be able to resolve by passing context as a first argument,
		// but it looks ugly
		let str = unsafe { nix_raw::err_msg(null_mut(), self.0, null_mut()) };
		Some(unsafe { CStr::from_ptr(str) }.to_string_lossy())

		// TODO: There is also nix_err_info_msg, but I don't understand when it should be used
		// Some(match self.error_kind()? {
		// 	NixErrorKind::Generic => {
		// 	}
		// })
	}
	fn clean_err(&mut self) {
		unsafe {
			nix_raw::clear_err(self.0);
		}
	}

	fn bail_if_error(&self) -> Result<()> {
		if let Some(err) = self.error() {
			bail!("{err}");
		};
		Ok(())
	}

	fn run_in_context<T>(&mut self, f: impl FnOnce(*mut c_context) -> T) -> Result<T> {
		self.clean_err();
		let o = f(self.0);
		self.bail_if_error()?;
		self.clean_err();
		Ok(o)
	}
}
impl Drop for NixContext {
	fn drop(&mut self) {
		unsafe {
			nix_raw::c_context_free(self.0);
		}
	}
}
struct GlobalState {
	store: Store,
	state: EvalState,
}
impl GlobalState {
	fn new() -> Result<Self> {
		let mut ctx = NixContext::new();
		let store = ctx
			.run_in_context(|c| unsafe { nix_raw::store_open(c, c"daemon".as_ptr(), null_mut()) })
			.map(Store)?;

		let builder = ctx.run_in_context(|c| unsafe { eval_state_builder_new(c, store.0) })?;
		ctx.run_in_context(|c| {
			unsafe {
				nix_raw::eval_state_builder_set_eval_setting(
					c,
					builder,
					c"lazy-trees".as_ptr(),
					c"true".as_ptr(),
				)
			}
			// eval_s
		})?;
		let state = ctx
			.run_in_context(|c| unsafe { eval_state_build(c, builder) })
			.map(EvalState)?;

		Ok(Self { store, state })
	}
}

struct ThreadState {
	ctx: NixContext,
}
impl ThreadState {
	fn new() -> Result<Self> {
		let ctx = NixContext::new();

		Ok(Self { ctx })
	}
}

static GLOBAL_STATE: LazyLock<GlobalState> =
	LazyLock::new(|| GlobalState::new().expect("global state init shouldn't fail"));

thread_local! {
	static THREAD_STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new().expect("thread state init shouldn't fail"));
}
fn with_default_context<T>(
	f: impl FnOnce(*mut c_context, *mut nix_raw::EvalState) -> T,
) -> Result<T> {
	let global = &GLOBAL_STATE.state;
	let (ctx, state) = THREAD_STATE.with_borrow_mut(|w| (w.ctx.0, global.0));
	let mut ctx = NixContext(ctx);
	let v = ctx.run_in_context(|c| f(c, state));
	// It is reused for thread
	std::mem::forget(ctx);
	v
}

fn set_setting(s: &CStr, v: &CStr) -> Result<()> {
	with_default_context(|c, _| unsafe { setting_set(c, s.as_ptr(), v.as_ptr()) }).map(|_| ())
}

pub struct FetchSettings(*mut fetchers_settings);
impl FetchSettings {
	pub fn new() -> Self {
		Self::try_new().expect("allocation should not fail")
	}
	fn try_new() -> Result<Self> {
		with_default_context(|c, _| unsafe { fetchers_settings_new(c) }).map(Self)
	}
	pub fn set(&mut self, setting: &CStr, value: &CStr) {
		unsafe {
			set_fetcher_setting(self.0.cast(), setting.as_ptr(), value.as_ptr());
		};
	}
}
unsafe impl Send for FetchSettings {}
unsafe impl Sync for FetchSettings {}
impl Drop for FetchSettings {
	fn drop(&mut self) {
		unsafe { fetchers_settings_free(self.0) };
	}
}
pub struct FlakeSettings(*mut flake_settings);
impl FlakeSettings {
	pub fn new() -> Result<Self> {
		with_default_context(|c, _| unsafe { flake_settings_new(c) }).map(Self)
	}
}
unsafe impl Send for FlakeSettings {}
unsafe impl Sync for FlakeSettings {}
impl Drop for FlakeSettings {
	fn drop(&mut self) {
		unsafe {
			flake_settings_free(self.0);
		}
	}
}

struct FlakeReferenceParseFlags(*mut flake_reference_parse_flags);
impl FlakeReferenceParseFlags {
	fn new(settings: &mut FlakeSettings) -> Result<Self> {
		with_default_context(|c, _| unsafe { flake_reference_parse_flags_new(c, settings.0) })
			.map(Self)
	}
	fn set_base_dir(&mut self, dir: &str) -> Result<()> {
		with_default_context(|c, _| {
			unsafe {
				flake_reference_parse_flags_set_base_directory(
					c,
					self.0,
					dir.as_ptr().cast(),
					dir.len(),
				)
			};
		})
	}
}
impl Drop for FlakeReferenceParseFlags {
	fn drop(&mut self) {
		unsafe {
			flake_reference_parse_flags_free(self.0);
		}
	}
}
struct FlakeLockFlags(*mut flake_lock_flags);
impl FlakeLockFlags {
	fn new(settings: &mut FlakeSettings) -> Result<Self> {
		with_default_context(|c, _| unsafe { flake_lock_flags_new(c, settings.0) }).map(Self)
	}
}
impl Drop for FlakeLockFlags {
	fn drop(&mut self) {
		unsafe {
			flake_lock_flags_free(self.0);
		}
	}
}

unsafe extern "C" fn copy_nix_str(start: *const c_char, n: c_uint, user_data: *mut c_void) {
	let s = unsafe { std::slice::from_raw_parts(start.cast::<u8>(), n as usize) };
	let s = std::str::from_utf8(s).expect("c string has invalid utf-8");
	unsafe { *user_data.cast::<String>() = s.to_owned() };
}

struct Store(*mut nix_raw::Store);
unsafe impl Send for Store {}
unsafe impl Sync for Store {}

struct EvalState(*mut nix_raw::EvalState);
impl EvalState {
	// TODO: store ownership
	fn new_raw(store: *mut nix_raw::Store) -> Result<Self> {
		let builder =
			with_default_context(|c, _| unsafe { nix_raw::eval_state_builder_new(c, store) })?;

		with_default_context(|c, _| unsafe { eval_state_build(c, builder) }).map(Self)

		// with_default_context(|c| state_create(c))
	}
}
unsafe impl Send for EvalState {}
unsafe impl Sync for EvalState {}
impl Drop for EvalState {
	fn drop(&mut self) {
		unsafe {
			state_free(self.0);
		}
	}
}

pub struct FlakeReference(*mut nix_raw::flake_reference);
impl FlakeReference {
	pub fn new(s: &str, fetch: &FetchSettings) -> Result<(Self, String)> {
		let mut flake_settings = FlakeSettings::new()?;
		let mut parse_flags = FlakeReferenceParseFlags::new(&mut flake_settings)?;

		// parse_flags.set_base_dir("/home/lach/build/fleet")?;

		let mut out = null_mut();
		let mut fragment = String::new();
		// let fetch_settings = fetcher_settings;
		with_default_context(|c, _| unsafe {
			nix_raw::flake_reference_and_fragment_from_string(
				c,
				fetch.0,
				flake_settings.0,
				parse_flags.0,
				s.as_ptr().cast(),
				s.len(),
				&mut out,
				Some(copy_nix_str),
				(&raw mut fragment).cast(),
			)
		})?;
		assert!(!out.is_null());

		Ok((Self(out), fragment))
	}
	pub fn lock(&mut self, fetch: &FetchSettings) -> Result<LockedFlake> {
		let mut settings = FlakeSettings::new()?;
		let lock_flags = FlakeLockFlags::new(&mut settings)?;
		with_default_context(|c, es| unsafe {
			flake_lock(c, fetch.0, settings.0, es, lock_flags.0, self.0)
		})
		.map(LockedFlake)
	}
}
unsafe impl Send for FlakeReference {}
unsafe impl Sync for FlakeReference {}

pub struct LockedFlake(*mut nix_raw::locked_flake);
impl LockedFlake {
	pub fn get_attrs(&self, settings: &mut FlakeSettings) -> Result<Value> {
		with_default_context(|c, es| unsafe {
			locked_flake_get_output_attrs(c, settings.0, es, self.0)
		})
		.map(Value)
	}
}
unsafe impl Send for LockedFlake {}
unsafe impl Sync for LockedFlake {}
impl Drop for LockedFlake {
	fn drop(&mut self) {
		unsafe {
			locked_flake_free(self.0);
		};
	}
}

type FieldName = [u8; 32];
fn init_field_name(v: &str) -> FieldName {
	let mut f = [0; 32];
	assert!(v.len() < 32, "max field name is 31 char");
	assert!(
		v.bytes().all(|v| v != 0),
		"nul bytes are unsupported in field name"
	);
	f[0..v.len()].copy_from_slice(v.as_bytes());
	f
}

pub struct RealisedString(*mut nix_raw::realised_string);
impl fmt::Debug for RealisedString {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.as_str().fmt(f)
	}
}

impl RealisedString {
	fn as_str(&self) -> &str {
		let len = unsafe { nix_raw::realised_string_get_buffer_size(self.0) };
		let data: *const u8 = unsafe { nix_raw::realised_string_get_buffer_start(self.0) }.cast();
		let data = unsafe { std::slice::from_raw_parts(data, len) };
		std::str::from_utf8(data).expect("non-utf8 strings not supported")
	}
	fn path_count(&self) -> usize {
		unsafe { nix_raw::realised_string_get_store_path_count(self.0) }
	}
	fn path(&self, i: usize) -> String {
		assert!(i < self.path_count());
		let path = unsafe { nix_raw::realised_string_get_store_path(self.0, i) };
		let mut err_out = String::new();
		unsafe { nix_raw::store_path_name(path, Some(copy_nix_str), (&raw mut err_out).cast()) };
		err_out
	}
}

unsafe impl Send for RealisedString {}
impl Drop for RealisedString {
	fn drop(&mut self) {
		with_default_context(|c, _| unsafe { nix_raw::realised_string_free(self.0) })
			.expect("string free should not fail")
	}
}

pub struct Value(*mut nix_raw::value);

unsafe impl Send for Value {}
unsafe impl Sync for Value {}

pub trait AsFieldName {
	fn as_field_name<T>(&self, v: impl FnOnce(FieldName) -> Result<T>) -> Result<T>;
	fn to_field_name(&self) -> Result<String>;
}
impl AsFieldName for Value {
	fn as_field_name<T>(&self, v: impl FnOnce(FieldName) -> Result<T>) -> Result<T> {
		let f = self.to_string()?;
		v(init_field_name(&f))
	}
	fn to_field_name(&self) -> Result<String> {
		self.to_string()
	}
}
impl<E> AsFieldName for E
where
	E: AsRef<str>,
{
	fn as_field_name<T>(&self, v: impl FnOnce(FieldName) -> Result<T>) -> Result<T> {
		let f = self.as_ref();
		v(init_field_name(f))
	}
	fn to_field_name(&self) -> Result<String> {
		Ok(self.as_ref().to_owned())
	}
}

struct AttrsBuilder(*mut nix_raw::BindingsBuilder);
impl AttrsBuilder {
	fn new(capacity: usize) -> Self {
		with_default_context(|c, es| unsafe { nix_raw::make_bindings_builder(c, es, capacity) })
			.map(Self)
			.expect("alloc should not fail")
	}
	fn insert(&mut self, k: &impl AsFieldName, v: Value) {
		k.as_field_name(|name| {
			with_default_context(|c, _| unsafe {
				nix_raw::bindings_builder_insert(c, self.0, name.as_ptr().cast(), v.0)
			})
		})
		.expect("builder insert shouldn't fail");
	}
}
impl Drop for AttrsBuilder {
	fn drop(&mut self) {
		unsafe { nix_raw::bindings_builder_free(self.0) };
	}
}

impl Value {
	pub fn new_attrs(v: HashMap<&str, Value>) -> Result<Self> {
		let out = Self::new_uninit()?;
		let mut b = AttrsBuilder::new(v.len());
		for (k, v) in v {
			b.insert(&k, v);
		}
		with_default_context(|c, _| unsafe { nix_raw::make_attrs(c, out.0, b.0) })?;
		Ok(out)
	}
	fn new_list<T: Into<Self>>(v: Vec<T>) -> Result<Self> {
		todo!()
	}
	fn new_uninit() -> Result<Self> {
		let out = with_default_context(|c, es| unsafe { alloc_value(c, es) })?;
		Ok(Self(out))
	}
	fn new_str(v: &str) -> Result<Self> {
		let s = CString::new(v).expect("string should not contain NULs");
		let uninit = Self::new_uninit()?;
		// String is copied, `s` is free to be dropped
		with_default_context(|c, _| unsafe { init_string(c, uninit.0, s.as_ptr()) })?;
		Ok(uninit)
	}
	fn new_int(i: i64) -> Result<Self> {
		let uninit = Self::new_uninit()?;
		with_default_context(|c, _| unsafe { init_int(c, uninit.0, i) })?;
		Ok(uninit)
	}
	fn new_bool(v: bool) -> Result<Self> {
		let uninit = Self::new_uninit()?;
		with_default_context(|c, _| unsafe { init_bool(c, uninit.0, v) })?;
		Ok(uninit)
	}
	fn force(&mut self, st: &mut EvalState) -> Result<()> {
		with_default_context(|c, _| unsafe { value_force(c, st.0, self.0) })?;
		Ok(())
	}
	pub fn type_of(&self) -> Result<NixType> {
		let ty = with_default_context(|c, _| unsafe { nix_raw::get_type(c, self.0) })?;
		Ok(NixType::from_int(ty))
	}
	pub fn to_string(&self) -> Result<String> {
		Ok(self.to_realised_string()?.as_str().to_owned())
	}
	pub fn to_realised_string(&self) -> Result<RealisedString> {
		with_default_context(|c, es| unsafe { nix_raw::string_realise(c, es, self.0, false) })
			.map(RealisedString)

		// let store_paths = unsafe { nix_raw::realised_string_get_store_path_count(str) };
		// for i in 0..store_paths {
		// 	let store_path = unsafe { nix_raw::realised_string_get_store_path(str, i) };
		// 	nix_raw::store_path_name(store_path, callback, user_data);
		// }
		// dbg!(store_paths);
		// todo!();
	}

	pub fn has_field(&self, field: &str) -> Result<bool> {
		let f = init_field_name(field);
		with_default_context(|c, es| unsafe {
			nix_raw::has_attr_byname(c, self.0, es, f.as_ptr().cast())
		})
	}
	// pub fn derivation_path(&self) {
	// 	nix_raw::real
	// }
	pub fn list_fields(&self) -> Result<Vec<String>> {
		if !matches!(self.type_of()?, NixType::Attrs) {
			bail!("invalid type: expected attrs");
		}

		let len = with_default_context(|c, _| unsafe { nix_raw::get_attrs_size(c, self.0) })?;
		let mut out = Vec::with_capacity(len as usize);

		for i in 0..len {
			let name = with_default_context(|c, es| unsafe {
				nix_raw::get_attr_name_byidx(c, self.0, es, i)
			})?;
			let c = unsafe { CStr::from_ptr(name) };
			out.push(c.to_str().expect("nix field names are utf-8").to_owned());
		}
		Ok(out)
	}
	pub fn get_elem(&self, v: usize) -> Result<Self> {
		if !matches!(self.type_of()?, NixType::List) {
			bail!("invalid type: expected list");
		}
		let len =
			with_default_context(|c, _| unsafe { nix_raw::get_list_size(c, self.0) })? as usize;
		if v >= len {
			bail!("oob list get: {v} >= {len}");
		}

		with_default_context(|c, es| unsafe { nix_raw::get_list_byidx(c, self.0, es, v as u32) })
			.map(Self)
	}
	pub fn attrs_update(self, other: Value) -> Result<Self> {
		let a_fields = self.list_fields()?;
		let b_fields = other.list_fields()?;
		match (a_fields.len(), b_fields.len()) {
			(_, 0) => return Ok(self),
			(0, _) => return Ok(other),
			_ => {}
		}
		let mut out = HashMap::new();
		for f in a_fields.iter() {
			if b_fields.contains(f) {
				break;
			}
			out.insert(f.as_str(), self.get_field(f)?);
		}
		if out.is_empty() {
			// All fields from lhs are overriden by rhs
			return Ok(other);
		}
		for f in b_fields.iter() {
			out.insert(f.as_str(), other.get_field(f)?);
		}
		Self::new_attrs(out)
	}
	pub fn get_field(&self, name: impl AsFieldName) -> Result<Self> {
		if !matches!(self.type_of()?, NixType::Attrs) {
			bail!("invalid type: expected attrs");
		}

		name.as_field_name(|name| {
			with_default_context(|c, es| unsafe {
				nix_raw::get_attr_byname(c, self.0, es, name.as_ptr().cast())
			})
			.map(Self)
		})
		.with_context(|| format!("getting field {:?}", name.to_field_name()))
	}
	pub fn call(&self, v: Value) -> Result<Self> {
		if !matches!(self.type_of()?, NixType::Function) {
			// TODO: Functors
			bail!("invalid type: expected function");
		}

		let out = Value::new_uninit()?;
		with_default_context(|c, es| unsafe { nix_raw::value_call(c, es, self.0, v.0, out.0) })?;

		Ok(out)
	}
	pub fn eval(v: &str) -> Result<Self> {
		let s = CString::new(v).expect("expression shouldn't have internal NULs");
		let out = Self::new_uninit()?;
		with_default_context(|c, es| unsafe {
			expr_eval_from_string(c, es, s.as_ptr(), c"/homeless-shelter".as_ptr(), out.0)
		})?;
		Ok(out)
	}
	pub async fn build(&self, output: &str) -> Result<PathBuf> {
		if !self.is_derivation() {
			bail!("expected derivation to build")
		}
		let output_name = self.get_field("outputName")?.to_string()?;
		let v = if output_name != output {
			let out = self.get_field(output)?;
			if !out.is_derivation() {
				bail!("unknown output: {output}");
			}
			out
		} else {
			self.clone()
		};
		// to_string here blocks until the path is built
		let drv_path = tokio::task::spawn_blocking(move || v.get_field("outPath")?.to_string())
			.await
			.expect("should not fail")?;
		Ok(PathBuf::from(drv_path))
	}
	pub fn as_json<T: DeserializeOwned>(&self) -> Result<T> {
		let to_json = Self::eval("builtins.toJSON")?;
		let s = to_json.call(self.clone())?.to_string()?;
		Ok(serde_json::from_str(&s)?)
	}
	pub fn serialized<T: Serialize>(v: &T) -> Result<Self> {
		Self::eval(&nixlike::serialize(v)?)
	}

	// Convert to string/evaluate derivations/etc
	fn to_string_weak(&self) -> Result<String> {
		// TODO
		self.to_string()
	}

	fn is_derivation(&self) -> bool {
		if !matches!(self.type_of(), Ok(NixType::Attrs)) {
			return false;
		}
		let Some(ty) = self.get_field("type").ok() else {
			return false;
		};
		matches!(ty.to_string().as_deref(), Ok("derivation"))
	}
}

impl From<String> for Value {
	fn from(value: String) -> Self {
		Value::new_str(&value).expect("todo: TryFrom")
	}
}
impl From<bool> for Value {
	fn from(value: bool) -> Self {
		Value::new_bool(value).expect("todo: TryFrom")
	}
}
impl From<&str> for Value {
	fn from(value: &str) -> Self {
		Value::new_str(&value).expect("todo: TryFrom")
	}
}
impl<T> From<Vec<T>> for Value
where
	T: Into<Value>,
{
	fn from(value: Vec<T>) -> Self {
		Value::new_list(value).expect("todo: TryFrom")
	}
}

impl Clone for Value {
	fn clone(&self) -> Self {
		with_default_context(|c, _| unsafe { value_incref(c, self.0) })
			.expect("value incref should not fail");
		Self(self.0)
	}
}
impl Drop for Value {
	fn drop(&mut self) {
		with_default_context(|c, _| unsafe { value_decref(c, self.0) })
			.expect("value drop should not fail");
	}
}

pub fn init_libraries() {
	unsafe { nix_raw::GC_allow_register_threads() };

	let mut ctx = NixContext::new();
	ctx.run_in_context(|c| unsafe { nix_raw::libutil_init(c) })
		.expect("util init should not fail");
	ctx.run_in_context(|c| unsafe { nix_raw::libstore_init(c) })
		.expect("store init should not fail");
	ctx.run_in_context(|c| unsafe { nix_raw::libexpr_init(c) })
		.expect("expr init should not fail");

	nix_logging_cxx::apply_tracing_logger();
}

#[test_log::test]
fn test_native() -> Result<()> {
	let mut fetch_settings = FetchSettings::new();
	fetch_settings.set(c"warn-dirty", c"false");
	//

	let (mut r, _) = FlakeReference::new("/home/lach/build/fleet", &fetch_settings)?;
	let locked = r.lock(&fetch_settings)?;
	let attrs = locked.get_attrs(&mut FlakeSettings::new()?)?;

	let builtins = Value::eval("builtins")?;
	dbg!(builtins.type_of()?);

	dbg!(attrs.type_of()?);
	dbg!(attrs.list_fields()?);
	dbg!(
		attrs
			.get_field("packages")?
			.get_field("x86_64-linux")?
			.get_field("fleet")?
			.get_field("outPath")?
			.to_string()
	);

	Ok(())
}

// struct NixBuildTask(Value, oneshot::Sender<Result<HashMap<String, PathBuf>>>);
//
// #[derive(Clone)]
// pub struct NixBuildBatch {
// 	tx: mpsc::UnboundedSender<NixBuildTask>,
// }
//
// #[instrument(skip(values))]
// async fn build_multiple(name: String, values: Vec<Value>) -> Result<()> {
// 	let builtins = Value::eval("builtins")?;
// 	let drv = nix_go!(builtins.derivation(Obj {
// 		// FIXME: pass system from localSystem or fleet args
// 		// system,
// 		name,
// 		builder: "/bin/sh",
// 		// we want nothing from this derivation, it is only used to perform multiple builds at once.
// 		args: vec!["-c", "echo > $out"],
// 		preferLocalBuild: true,
// 		allowSubstitutes: false,
// 		buildInputs: values,
// 	}));
// 	drv.build()?;
// 	Ok(())
// }
//
// impl NixBuildBatch {
// 	fn new(name: String) -> Self {
// 		let (tx, mut rx) = mpsc::unbounded_channel::<NixBuildTask>();
//
// 		tokio::task::spawn(async move {
// 			let mut deps = vec![];
// 			let mut build_data = vec![];
// 			while let Some(task) = rx.recv().await {
// 				build_data.push(task.0.clone());
// 				deps.push(task);
// 			}
// 			if deps.is_empty() {
// 				return;
// 			}
// 			match build_multiple(name, build_data).await {
// 				Ok(_) => {
// 					for NixBuildTask(v, o) in deps {
// 						let _ = o.send(v.build());
// 					}
// 				}
// 				Err(e) => {
// 					for NixBuildTask(v, o) in deps {
// 						let s = v.to_string_weak();
// 						let s = match s {
// 							Ok(s) => s,
// 							Err(e) => {
// 								let _ = o.send(Err(e));
// 								continue;
// 							}
// 						};
// 						if PathBuf::from(s).exists() {
// 							let _ = o.send(v.build());
// 						} else {
// 							let _ = o.send(Err(e.clone()));
// 						}
// 					}
// 				}
// 			};
// 		});
// 		Self { tx }
// 	}
// 	pub async fn submit(self, task: Value) -> Result<HashMap<String, PathBuf>> {
// 		let Self { tx: task_tx } = self;
// 		let (tx, rx) = oneshot::channel();
// 		let _ = task_tx.send(NixBuildTask(task, tx));
// 		drop(task_tx);
// 		rx.await.expect("shoudn't be cancelled here")
// 	}
// }
