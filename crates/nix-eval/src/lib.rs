use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::ptr::null_mut;
use std::sync::LazyLock;
use std::{collections::HashMap, path::PathBuf};
use std::{fmt, slice};

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use anyhow::Result;
use tracing::instrument;

use self::logging::nix_logging_cxx;
use self::nix_cxx::set_fetcher_setting;
use self::nix_raw::{
	BindingsBuilder as c_bindings_builder, EvalState as c_eval_state, GC_SUCCESS,
	GC_allow_register_threads, GC_get_stack_base, GC_register_my_thread, GC_stack_base,
	GC_thread_is_registered, GC_unregister_my_thread, ListBuilder as c_list_builder,
	Store as c_store, StorePath as c_store_path, alloc_value, bindings_builder_free,
	bindings_builder_insert, c_context, c_context_create, c_context_free, clear_err, err_code,
	err_info_msg, err_msg, eval_state_build, eval_state_builder_load, eval_state_builder_new,
	eval_state_builder_set_eval_setting, expr_eval_from_string, fetchers_settings,
	fetchers_settings_free, fetchers_settings_new, flake_lock, flake_lock_flags,
	flake_lock_flags_free, flake_lock_flags_new, flake_reference,
	flake_reference_and_fragment_from_string, flake_reference_parse_flags,
	flake_reference_parse_flags_free, flake_reference_parse_flags_new,
	flake_reference_parse_flags_set_base_directory, flake_settings, flake_settings_free,
	flake_settings_new, gc_now as gc_now_raw, get_attr_byname, get_attr_name_byidx, get_attrs_size,
	get_list_byidx, get_list_size, get_string, get_type, has_attr_byname, init_bool, init_int,
	init_string, libexpr_init, libstore_init, libutil_init, list_builder_free, list_builder_insert,
	locked_flake, locked_flake_free, locked_flake_get_output_attrs, make_attrs,
	make_bindings_builder, make_list, make_list_builder, realised_string, realised_string_free,
	realised_string_get_buffer_size, realised_string_get_buffer_start,
	realised_string_get_store_path, realised_string_get_store_path_count, set_err_msg, setting_set,
	state_free, store_open, store_parse_path, store_path_free, store_path_name, string_realise,
	value, value_call, value_decref, value_incref,
};

// Contains macros helpers
pub mod logging;
#[doc(hidden)]
pub mod macros;
pub mod util;

#[allow(
	non_upper_case_globals,
	non_camel_case_types,
	non_snake_case,
	dead_code
)]
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

enum FunctorKind {
	Function,
	Functor,
}

#[derive(Debug)]
#[repr(i32)]
pub enum NixErrorKind {
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

pub fn gc_now() {
	unsafe { gc_now_raw() };
}

pub fn gc_register_my_thread() {
	assert_eq!(unsafe { GC_thread_is_registered() }, 0);

	let mut sb = GC_stack_base {
		mem_base: null_mut(),
	};
	let r = unsafe { GC_get_stack_base(&mut sb) };
	if r as u32 != GC_SUCCESS {
		panic!("failed to get thread stack base");
	}
	unsafe { GC_register_my_thread(&sb) };
}
pub fn gc_unregister_my_thread() {
	assert_eq!(unsafe { GC_thread_is_registered() }, 1);

	unsafe { GC_unregister_my_thread() };
}

pub struct ThreadRegisterGuard {}
impl ThreadRegisterGuard {
	#[allow(clippy::new_without_default)]
	pub fn new() -> Self {
		gc_register_my_thread();
		Self {}
	}
}
impl Drop for ThreadRegisterGuard {
	fn drop(&mut self) {
		gc_unregister_my_thread();
	}
}

pub struct NixContext(*mut c_context);
impl NixContext {
	pub fn set_err(&mut self, err: NixErrorKind, msg: &CStr) {
		unsafe { set_err_msg(self.0, err as c_int, msg.as_ptr()) };
	}
	pub fn new() -> Self {
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
		let str = unsafe { err_msg(null_mut(), self.0, null_mut()) };
		Some(unsafe { CStr::from_ptr(str) }.to_string_lossy())
	}
	fn clean_err(&mut self) {
		unsafe {
			clear_err(self.0);
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

impl Default for NixContext {
	fn default() -> Self {
		Self::new()
	}
}
impl Drop for NixContext {
	fn drop(&mut self) {
		unsafe {
			c_context_free(self.0);
		}
	}
}
struct GlobalState {
	// Store should be valid as long as EvalState is valid
	#[allow(dead_code)]
	store: Store,
	state: EvalState,
}
impl GlobalState {
	fn new() -> Result<Self> {
		let mut ctx = NixContext::new();
		let store = ctx
			.run_in_context(|c| unsafe { store_open(c, c"auto".as_ptr(), null_mut()) })
			.map(Store)?;

		let builder = ctx.run_in_context(|c| unsafe { eval_state_builder_new(c, store.0) })?;
		ctx.run_in_context(|c| unsafe { eval_state_builder_load(c, builder) })?;
		ctx.run_in_context(|c| unsafe {
			eval_state_builder_set_eval_setting(
				c,
				builder,
				c"lazy-trees".as_ptr(),
				c"true".as_ptr(),
			)
		})?;
		ctx.run_in_context(|c| unsafe {
			eval_state_builder_set_eval_setting(
				c,
				builder,
				c"lazy-locks".as_ptr(),
				c"true".as_ptr(),
			)
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
fn with_default_context<T>(f: impl FnOnce(*mut c_context, *mut c_eval_state) -> T) -> Result<T> {
	let global = &GLOBAL_STATE.state;
	let (ctx, state) = THREAD_STATE.with_borrow_mut(|w| (w.ctx.0, global.0));
	let mut ctx = NixContext(ctx);
	let v = ctx.run_in_context(|c| f(c, state));
	// It is reused for thread
	std::mem::forget(ctx);
	v
}

pub fn set_setting(s: &CStr, v: &CStr) -> Result<()> {
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

impl Default for FetchSettings {
	fn default() -> Self {
		Self::new()
	}
}

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

pub struct FlakeReferenceParseFlags(*mut flake_reference_parse_flags);
impl FlakeReferenceParseFlags {
	pub fn new(settings: &FlakeSettings) -> Result<Self> {
		with_default_context(|c, _| unsafe { flake_reference_parse_flags_new(c, settings.0) })
			.map(Self)
	}
	pub fn set_base_dir(&mut self, dir: &str) -> Result<()> {
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
pub struct FlakeLockFlags(*mut flake_lock_flags);
impl FlakeLockFlags {
	pub fn new(settings: &FlakeSettings) -> Result<Self> {
		let o = with_default_context(|c, _| unsafe { flake_lock_flags_new(c, settings.0) })
			.map(Self)?;
		// with_default_context(|c, _| unsafe { flake_lock_flags_set_mode_virtual(c, o.0) })?;

		Ok(o)
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
	let s = unsafe { slice::from_raw_parts(start.cast::<u8>(), n as usize) };
	let s = std::str::from_utf8(s).expect("c string has invalid utf-8");
	unsafe { *user_data.cast::<String>() = s.to_owned() };
}

struct Store(*mut c_store);
unsafe impl Send for Store {}
unsafe impl Sync for Store {}

impl Store {
	fn parse_path(&self, path: &CStr) -> Result<StorePath> {
		with_default_context(|c, _| {
			StorePath(unsafe { store_parse_path(c, self.0, path.as_ptr()) })
		})
	}
}

struct EvalState(*mut c_eval_state);
unsafe impl Send for EvalState {}
unsafe impl Sync for EvalState {}

impl Drop for EvalState {
	fn drop(&mut self) {
		unsafe {
			state_free(self.0);
		}
	}
}

pub struct FlakeReference(*mut flake_reference);
impl FlakeReference {
	#[instrument(name = "new-flake-reference", skip(flake, parse, fetch))]
	pub fn new(
		s: &str,
		flake: &FlakeSettings,
		parse: &FlakeReferenceParseFlags,
		fetch: &FetchSettings,
	) -> Result<(Self, String)> {
		let mut out = null_mut();
		let mut fragment = String::new();
		// let fetch_settings = fetcher_settings;
		with_default_context(|c, _| unsafe {
			flake_reference_and_fragment_from_string(
				c,
				fetch.0,
				flake.0,
				parse.0,
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
	#[instrument(name = "lock-flake", skip(self, fetch, flake, lock))]
	pub fn lock(
		&mut self,
		fetch: &FetchSettings,
		flake: &FlakeSettings,
		lock: &FlakeLockFlags,
	) -> Result<LockedFlake> {
		with_default_context(|c, es| unsafe { flake_lock(c, fetch.0, flake.0, es, lock.0, self.0) })
			.map(LockedFlake)
	}
}
unsafe impl Send for FlakeReference {}
unsafe impl Sync for FlakeReference {}

pub struct LockedFlake(*mut locked_flake);
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

type FieldName = [u8; 64];
fn init_field_name(v: &str) -> FieldName {
	let mut f = [0; 64];
	assert!(v.len() < 64, "max field name is 63 chars");
	assert!(
		v.bytes().all(|v| v != 0),
		"nul bytes are unsupported in field name"
	);
	f[0..v.len()].copy_from_slice(v.as_bytes());
	f
}

pub struct RealisedString(*mut realised_string);
impl fmt::Debug for RealisedString {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.as_str().fmt(f)
	}
}

impl RealisedString {
	pub fn as_str(&self) -> &str {
		let len = unsafe { realised_string_get_buffer_size(self.0) };
		let data: *const u8 = unsafe { realised_string_get_buffer_start(self.0) }.cast();
		let data = unsafe { slice::from_raw_parts(data, len) };
		std::str::from_utf8(data).expect("non-utf8 strings not supported")
	}
	pub fn path_count(&self) -> usize {
		unsafe { realised_string_get_store_path_count(self.0) }
	}
	pub fn path(&self, i: usize) -> String {
		assert!(i < self.path_count());
		let path = unsafe { realised_string_get_store_path(self.0, i) };
		let mut err_out = String::new();
		unsafe { store_path_name(path, Some(copy_nix_str), (&raw mut err_out).cast()) };
		err_out
	}
}

unsafe impl Send for RealisedString {}
impl Drop for RealisedString {
	fn drop(&mut self) {
		unsafe { realised_string_free(self.0) }
	}
}

pub struct Value(*mut value);

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

struct AttrsBuilder(*mut c_bindings_builder);
impl AttrsBuilder {
	fn new(capacity: usize) -> Self {
		with_default_context(|c, es| unsafe { make_bindings_builder(c, es, capacity) })
			.map(Self)
			.expect("alloc should not fail")
	}
	fn insert(&mut self, k: &impl AsFieldName, v: Value) {
		k.as_field_name(|name| {
			with_default_context(|c, _| unsafe {
				bindings_builder_insert(c, self.0, name.as_ptr().cast(), v.0);
				// bindings_builder_insert doesn't do incref
			})
		})
		.expect("builder insert shouldn't fail");
	}
}
impl Drop for AttrsBuilder {
	fn drop(&mut self) {
		unsafe { bindings_builder_free(self.0) };
	}
}

struct ListBuilder(*mut c_list_builder, c_uint);
impl ListBuilder {
	fn new(capacity: usize) -> Self {
		with_default_context(|c, es| unsafe { make_list_builder(c, es, capacity) })
			.map(|l| Self(l, 0))
			.expect("alloc should not fail")
	}
}
impl ListBuilder {
	fn push(&mut self, v: Value) {
		with_default_context(|c, _| unsafe {
			list_builder_insert(
				c,
				self.0,
				{
					let v = self.1;
					self.1 += 1;
					v
				},
				v.0,
			)
		})
		.expect("list insert shouldn't fail");
	}
}
impl Drop for ListBuilder {
	fn drop(&mut self) {
		unsafe { list_builder_free(self.0) };
	}
}

impl Value {
	pub fn new_attrs(v: HashMap<&str, Value>) -> Self {
		let out = Self::new_uninit();
		let mut b = AttrsBuilder::new(v.len());
		for (k, v) in v {
			b.insert(&k, v);
		}
		with_default_context(|c, _| unsafe { make_attrs(c, out.0, b.0) })
			.expect("attrs initialization should not fail");

		out
	}
	fn new_list<T: Into<Self>>(v: Vec<T>) -> Self {
		let out = Self::new_uninit();
		let mut b = ListBuilder::new(v.len());
		for v in v {
			b.push(v.into());
		}
		with_default_context(|c, _| unsafe { make_list(c, b.0, out.0) })
			.expect("list initialization should not fail");

		out
	}
	fn new_uninit() -> Self {
		let out = with_default_context(|c, es| unsafe { alloc_value(c, es) })
			.expect("value allocation should not fail");
		Self(out)
	}
	pub fn new_str(v: &str) -> Self {
		let s = CString::new(v).expect("string should not contain NULs");
		let out = Self::new_uninit();
		// String is copied, `s` is free to be dropped
		with_default_context(|c, _| unsafe { init_string(c, out.0, s.as_ptr()) })
			.expect("string initialization should not fail");
		out
	}
	pub fn new_int(i: i64) -> Self {
		let out = Self::new_uninit();
		with_default_context(|c, _| unsafe { init_int(c, out.0, i) })
			.expect("int initialization should not fail");
		out
	}
	pub fn new_bool(v: bool) -> Self {
		let out = Self::new_uninit();
		with_default_context(|c, _| unsafe { init_bool(c, out.0, v) })
			.expect("bool initialization should not fail");
		out
	}
	// TODO: As far as I can see, there is no way to get Thunks from nix public C api, so this function is useless
	// fn force(&mut self, st: &mut EvalState) -> Result<()> {
	// 	with_default_context(|c, _| unsafe { value_force(c, st.0, self.0) })?;
	// 	Ok(())
	// }
	pub fn type_of(&self) -> NixType {
		let ty = with_default_context(|c, _| unsafe { get_type(c, self.0) })
			.expect("get_type should not fail");
		NixType::from_int(ty)
	}
	fn builtin_to_string(&self) -> Result<Self> {
		let builtin = Self::eval("builtins.toString")?;
		builtin.call(self.clone())
	}
	pub fn to_string(&self) -> Result<String> {
		let mut str_out = String::new();
		with_default_context(|c, _| unsafe {
			get_string(c, self.0, Some(copy_nix_str), (&raw mut str_out).cast())
		})?;

		Ok(str_out)
	}
	pub fn to_realised_string(&self) -> Result<RealisedString> {
		with_default_context(|c, es| unsafe { string_realise(c, es, self.0, false) })
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
		with_default_context(|c, es| unsafe { has_attr_byname(c, self.0, es, f.as_ptr().cast()) })
	}
	// pub fn derivation_path(&self) {
	// 	nix_raw::real
	// }
	pub fn list_fields(&self) -> Result<Vec<String>> {
		if !matches!(self.type_of(), NixType::Attrs) {
			bail!("invalid type: expected attrs");
		}

		let len = with_default_context(|c, _| unsafe { get_attrs_size(c, self.0) })?;
		let mut out = Vec::with_capacity(len as usize);

		for i in 0..len {
			let name =
				with_default_context(|c, es| unsafe { get_attr_name_byidx(c, self.0, es, i) })?;
			let c = unsafe { CStr::from_ptr(name) };
			out.push(c.to_str().expect("nix field names are utf-8").to_owned());
		}
		Ok(out)
	}
	pub fn get_elem(&self, v: usize) -> Result<Self> {
		if !matches!(self.type_of(), NixType::List) {
			bail!("invalid type: expected list");
		}
		let len = with_default_context(|c, _| unsafe { get_list_size(c, self.0) })? as usize;
		if v >= len {
			bail!("oob list get: {v} >= {len}");
		}

		with_default_context(|c, es| unsafe { get_list_byidx(c, self.0, es, v as u32) }).map(Self)
	}
	pub fn attrs_update(self, other: Value/*, ignore_errors: bool*/) -> Result<Self> {
		let attrs_update_fn = Self::eval("a: b: a // b")?;

		attrs_update_fn.call(self)?.call(other).context("attrs update")
	}
	pub fn get_field(&self, name: impl AsFieldName) -> Result<Self> {
		if !matches!(self.type_of(), NixType::Attrs) {
			bail!("invalid type: expected attrs");
		}

		name.as_field_name(|name| {
			with_default_context(|c, es| unsafe {
				get_attr_byname(c, self.0, es, name.as_ptr().cast())
			})
			.map(Self)
		})
		.with_context(|| format!("getting field {:?}", name.to_field_name()))
	}
	pub fn call(&self, v: Value) -> Result<Self> {
		let kind = self
			.functor_kind()
			.ok_or_else(|| anyhow!("can only call function or functor"))?;

		let function = match kind {
			FunctorKind::Function => self.clone(),
			FunctorKind::Functor => {
				let f = self
					.get_field("__functor")
					.context("getting functor value")?;
				assert_eq!(
					f.type_of(),
					NixType::Function,
					"invalid functor encountered"
				);
				f
			}
		};

		let out = Value::new_uninit();
		with_default_context(|c, es| unsafe { value_call(c, es, function.0, v.0, out.0) })?;

		Ok(out)
	}
	pub fn eval(v: &str) -> Result<Self> {
		let s = CString::new(v).expect("expression shouldn't have internal NULs");
		let out = Self::new_uninit();
		with_default_context(|c, es| unsafe {
			expr_eval_from_string(c, es, s.as_ptr(), c"/root".as_ptr(), out.0)
		})?;
		Ok(out)
	}
	pub fn build(&self, output: &str) -> Result<PathBuf> {
		if !self.is_derivation() {
			bail!("expected derivation to build")
		}
		let output_name = self
			.get_field("outputName")
			.context("getting output name field")?
			.to_string()?;
		let v = if output_name != output {
			let out = self.get_field(output).context("getting target output")?;
			if !out.is_derivation() {
				bail!("unknown output: {output}");
			}
			out
		} else {
			self.clone()
		};
		// to_string here blocks until the path is built
		let s = v.builtin_to_string()?;
		let rs = s.to_realised_string()?;
		let drv_path = rs.as_str().to_owned();
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
	// fn to_string_weak(&self) -> Result<String> {
	// 	// TODO: For now, it works exactly like to_string, see the comment for fn force()
	// 	self.to_string()
	// }

	fn is_derivation(&self) -> bool {
		if !matches!(self.type_of(), NixType::Attrs) {
			return false;
		}
		let Some(ty) = self.get_field("type").ok() else {
			return false;
		};
		matches!(ty.to_string().as_deref(), Ok("derivation"))
	}
	fn functor_kind(&self) -> Option<FunctorKind> {
		match self.type_of() {
			NixType::Attrs => self
				.has_field("__functor")
				.expect("has_field shouldn't fail for attrs")
				.then_some(FunctorKind::Functor),
			NixType::Function => Some(FunctorKind::Function),
			_ => None,
		}
	}
	pub fn is_function(&self) -> bool {
		self.functor_kind().is_some()
	}
}

impl From<String> for Value {
	fn from(value: String) -> Self {
		Value::new_str(&value)
	}
}
impl From<bool> for Value {
	fn from(value: bool) -> Self {
		Value::new_bool(value)
	}
}
impl From<&str> for Value {
	fn from(value: &str) -> Self {
		Value::new_str(value)
	}
}
impl<T> From<Vec<T>> for Value
where
	T: Into<Value>,
{
	fn from(value: Vec<T>) -> Self {
		Value::new_list(value)
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
	unsafe { GC_allow_register_threads() };

	let mut ctx = NixContext::new();
	ctx.run_in_context(|c| unsafe { libutil_init(c) })
		.expect("util init should not fail");
	ctx.run_in_context(|c| unsafe { libstore_init(c) })
		.expect("store init should not fail");
	ctx.run_in_context(|c| unsafe { libexpr_init(c) })
		.expect("expr init should not fail");

	nix_logging_cxx::apply_tracing_logger();
}

struct StorePath(*mut c_store_path);
impl StorePath {}

impl Drop for StorePath {
	fn drop(&mut self) {
		unsafe { store_path_free(self.0) }
	}
}

#[test_log::test]
fn test_native() -> Result<()> {
	init_libraries();

	let mut fetch_settings = FetchSettings::new();
	fetch_settings.set(c"warn-dirty", c"false");

	let manifest = format!("git+file://{}/../../", env!("CARGO_MANIFEST_DIR"));
	let flake = FlakeSettings::new()?;
	let parse = FlakeReferenceParseFlags::new(&flake)?;
	let (mut r, _) = FlakeReference::new(&manifest, &flake, &parse, &fetch_settings)?;
	let lock = FlakeLockFlags::new(&flake)?;
	let locked = r.lock(&fetch_settings, &flake, &lock)?;
	let attrs = locked.get_attrs(&mut FlakeSettings::new()?)?;

	let builtins = Value::eval("builtins")?;
	assert_eq!(builtins.type_of(), NixType::Attrs);

	assert_eq!(attrs.type_of(), NixType::Attrs);
	let test_data = nix_go!(attrs.testData);

	let test_string: String = nix_go_json!(test_data.testString);
	assert_eq!(test_string, "hello");

	let s = nix_go!(attrs.packages["x86_64-linux"].fleet.drvPath);
	let s = CString::new(s.to_string()?).expect("path str is cstring");

	let nix_ctx = NixContext::new();
	let store = GLOBAL_STATE.store.parse_path(s.as_c_str())?;

	// nix_raw::store_get_fs_closure(1);

	Ok(())
}

// pub struct GcAlloc;
// unsafe impl GlobalAlloc for GcAlloc {
// 	unsafe fn alloc(&self, l: Layout) -> *mut u8 {
// 		let ptr = unsafe { GC_malloc(l.size()) };
// 		ptr.cast()
// 	}
// 	unsafe fn dealloc(&self, ptr: *mut u8, _: Layout) {
// 		// unsafe { GC_free(ptr.cast()) };
// 	}
//
// 	unsafe fn realloc(&self, ptr: *mut u8, _: Layout, new_size: usize) -> *mut u8 {
// 		let ptr = unsafe { GC_realloc(ptr.cast(), new_size) };
// 		ptr.cast()
// 	}
// }
//
// #[global_allocator]
// static GC: GcAlloc = GcAlloc;
