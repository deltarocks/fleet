use std::collections::HashMap;
use std::fmt::Arguments;
use std::sync::{LazyLock, Mutex};

use tracing::{
	Level, Metadata, Span, debug, debug_span, error, error_span, event, info, info_span, trace,
	trace_span, warn, warn_span,
};
use tracing_indicatif::span_ext::IndicatifSpanExt as _;

#[derive(Debug)]
enum ActivityType {
	Unknown = 0,
	CopyPath = 100,
	FileTransfer = 101,
	Realise = 102,
	CopyPaths = 103,
	Builds = 104,
	Build = 105,
	OptimiseStore = 106,
	VerifyPaths = 107,
	Substitute = 108,
	QueryPathInfo = 109,
	PostBuildHook = 110,
	BuildWaiting = 111,
	FetchTree = 112,
}

fn strip_prefix_suffix<'s, 'p>(a: &'s str, pref: &'p str, suff: &'p str) -> Option<&'s str> {
	a.strip_prefix(pref)?.strip_suffix(suff)
}

fn parse_path(path: &str) -> &str {
	let path = strip_prefix_suffix(path, "\x1b[35;1m", "\x1b[0m").unwrap_or(path);
	path
}

fn parse_drv(drv: &str) -> &str {
	let drv = parse_path(drv);
	if let Some(pkg) = drv.strip_prefix("/nix/store/") {
		let mut it = pkg.splitn(2, '-');
		it.next();
		if let Some(pkg) = it.next() {
			return pkg;
		}
	}
	drv
}
fn parse_host(host: &str) -> &str {
	if host.is_empty() || host == "local" {
		return "local";
	}
	// https/ssh is the default
	host.strip_prefix("https://").unwrap_or(host)
}

impl ActivityType {
	fn name(&self) -> &'static str {
		match self {
			ActivityType::Unknown => "nix",
			ActivityType::CopyPath => "nix::copy-path",
			ActivityType::FileTransfer => "nix::file-transfer",
			ActivityType::Realise => "nix::realise",
			ActivityType::CopyPaths => "nix::copy-paths",
			ActivityType::Builds => "nix::builds",
			ActivityType::Build => "nix::build",
			ActivityType::OptimiseStore => "nix::optimise-store",
			ActivityType::VerifyPaths => "nix::verify-paths",
			ActivityType::Substitute => "nix::substitute",
			ActivityType::QueryPathInfo => "nix::query-path-info",
			ActivityType::PostBuildHook => "nix::post-build-hook",
			ActivityType::BuildWaiting => "nix::build-waiting",
			ActivityType::FetchTree => "nix::fetch-tree",
		}
	}
	fn format(
		&self,
		values: &[FieldValue],
		s: &str,
		into: impl FnOnce(Arguments<'_>) -> Span,
	) -> Span {
		use FieldValue::*;
		match (self, values) {
			(ActivityType::QueryPathInfo, [Str(drv), Str(host)]) => {
				let drv = parse_drv(drv);
				let host = parse_host(host);
				debug_span!(target: "nix::query-path-info", "querying", drv, host)
			}
			(ActivityType::Substitute, [Str(drv), Str(host)]) => {
				let drv = parse_drv(drv);
				let host = parse_host(host);
				debug_span!(target: "nix::substitute", "substituting", drv, host)
			}
			(ActivityType::CopyPath, [Str(drv), Str(from), Str(to)]) => {
				let drv = parse_drv(drv);
				let from = parse_host(from);
				let to = parse_host(to);
				debug_span!(target: "nix::copy-path", "copying", drv, from, to)
			}
			(ActivityType::Build, [Str(drv), Str(host), Int(_), Int(_)]) => {
				let drv = parse_drv(drv);
				let host = parse_host(host);
				info_span!(target: "nix::build", "building", drv, host)
			}
			(ActivityType::FileTransfer, [Str(file)]) => {
				info_span!(target: "nix::file-transfer", "downloading", file)
			}
			(ActivityType::Realise, []) => {
				debug_span!(target: "nix::realise", "realising")
			}
			(ActivityType::CopyPaths, []) => {
				debug_span!(target: "nix::copy-paths", "copying paths")
			}
			(ActivityType::Unknown, [])
				if s.starts_with("copying \"") && s.ends_with("\" to the store") =>
			{
				let tree = s
					.trim_start_matches("copying \"")
					.trim_end_matches("\" to the store");
				debug_span!(target: "nix::trees", "copying", tree)
			}
			(ActivityType::Unknown, [])
				if s.starts_with("copying '") && s.ends_with("' to the store") =>
			{
				let tree = s
					.trim_start_matches("copying '")
					.trim_end_matches("' to the store");
				debug_span!(target: "nix::trees", "copying", tree)
			}
			(ActivityType::Unknown, []) if s.starts_with("hashing '") && s.ends_with("'") => {
				let tree = s.trim_start_matches("hashing '").trim_end_matches("'");
				debug_span!(target: "nix::trees", "hashing", tree)
			}
			(ActivityType::Unknown, []) if s.starts_with("connecting to '") && s.ends_with("'") => {
				let host = s
					.trim_start_matches("connecting to '")
					.trim_end_matches("'");
				debug_span!(target: "nix::remote", "connecting", host)
			}
			(ActivityType::Unknown, [])
				if s.starts_with("copying outputs from '") && s.ends_with("'") =>
			{
				let host = s
					.trim_start_matches("copying outputs from '")
					.trim_end_matches("'");
				debug_span!(target: "nix::remote", "copying outputs", host)
			}
			(ActivityType::Unknown, [])
				if s.starts_with("copying dependencies to '") && s.ends_with("'") =>
			{
				let host = s
					.trim_start_matches("copying dependencies to '")
					.trim_end_matches("'");
				debug_span!(target: "nix::remote", "copying dependencies", host)
			}
			(ActivityType::Unknown, [])
				if s.starts_with("waiting for the upload lock to '") && s.ends_with("'") =>
			{
				let host = s
					.trim_start_matches("waiting for the upload lock to '")
					.trim_end_matches("'");
				debug_span!(target: "nix::remote", "waiting for upload lock", host)
			}
			(ActivityType::BuildWaiting, [])
				if s.starts_with("waiting for a machine to build '") && s.ends_with("'") =>
			{
				let drv = parse_drv(
					s.trim_start_matches("waiting for a machine to build '")
						.trim_end_matches("'"),
				);
				debug_span!(target: "nix::build-waiting", "waiting for available builder", drv)
			}
			(ActivityType::Unknown, []) if s == "querying info about missing paths" => {
				debug_span!(target: "nix::remote", "querying")
			}
			_ => into(format_args!("{}({values:?})", self.name())),
		}
	}
	fn from_int(v: u32) -> Self {
		match v {
			0 => Self::Unknown,
			100 => Self::CopyPath,
			101 => Self::FileTransfer,
			102 => Self::Realise,
			103 => Self::CopyPaths,
			104 => Self::Builds,
			105 => Self::Build,
			106 => Self::OptimiseStore,
			107 => Self::VerifyPaths,
			108 => Self::Substitute,
			109 => Self::QueryPathInfo,
			110 => Self::PostBuildHook,
			111 => Self::BuildWaiting,
			112 => Self::FetchTree,
			_ => {
				warn!("unknown nix action: {v}");
				Self::Unknown
			}
		}
	}
}

#[derive(Debug)]
enum ResultType {
	FileLinked = 100,
	BuildLogLine = 101,
	UntrustedPath = 102,
	CorruptedPath = 103,
	SetPhase = 104,
	Progress = 105,
	SetExpected = 106,
	PostBuildLogLine = 107,
	FetchStatus = 108,

	Unknown = 999,
}
impl ResultType {
	fn from_int(v: u32) -> Self {
		match v {
			100 => Self::FileLinked,
			101 => Self::BuildLogLine,
			102 => Self::UntrustedPath,
			103 => Self::CorruptedPath,
			104 => Self::SetPhase,
			105 => Self::Progress,
			106 => Self::SetExpected,
			107 => Self::PostBuildLogLine,
			108 => Self::FetchStatus,

			_ => {
				warn!("unknown nix result: {v}");
				Self::Unknown
			}
		}
	}
}
#[derive(Clone, Copy)]
enum Verbosity {
	Error,
	Warn,
	Notice,
	Info,
	Talkative,
	Chatty,
	Debug,
	Vomit,
}
impl Into<tracing::Level> for Verbosity {
	fn into(self) -> tracing::Level {
		match self {
			Verbosity::Error => Level::ERROR,
			Verbosity::Warn => Level::WARN,
			Verbosity::Notice => Level::WARN,
			Verbosity::Info => Level::INFO,
			Verbosity::Talkative => Level::DEBUG,
			Verbosity::Chatty => Level::DEBUG,
			Verbosity::Debug => Level::DEBUG,
			Verbosity::Vomit => Level::TRACE,
		}
	}
}
impl Verbosity {
	fn from_int(u: u32) -> Self {
		[
			Self::Error,
			Self::Warn,
			Self::Notice,
			Self::Info,
			Self::Talkative,
			Self::Chatty,
			Self::Debug,
			Self::Vomit,
		]
		.get(u as usize)
		.cloned()
		.unwrap_or_else(|| {
			warn!("unknown log level: {u}");
			Verbosity::Vomit
		})
	}
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
enum MetadataKind {
	Span,
	Event,
}
// impl MetadataKind {
// 	fn kind(&self) -> Kind {
// 		match self {
// 			MetadataKind::Span => Kind::SPAN,
// 			MetadataKind::Event => Kind::EVENT,
// 		}
// 	}
// }

#[derive(Hash, PartialEq, Eq)]
struct ForeignMetadataInfo {
	target: &'static str,
	level: Level,
	kind: MetadataKind,
	name: &'static str,
	module: Option<&'static str>,
	file: Option<&'static str>,
	line: Option<u32>,
	names: &'static [&'static str],
}

struct FakeCallsite;
impl tracing::callsite::Callsite for FakeCallsite {
	fn set_interest(&self, interest: tracing::subscriber::Interest) {
		unreachable!()
	}

	fn metadata(&self) -> &Metadata<'_> {
		unreachable!()
	}
}
const FAKE_CALLSITE: FakeCallsite = FakeCallsite;

#[cfg(false)]
#[derive(Default)]
struct ForeignSpanData {
	interned: HashSet<&'static str>,
	metadatas: HashMap<ForeignMetadataInfo, &'static Metadata<'static>>,
}
#[cfg(false)]
impl ForeignSpanData {
	fn intern(&mut self, s: &str) -> &'static str {
		if let Some(v) = self.interned.get(s) {
			return *v;
		}
		let leaked: Box<str> = s.into();
		let leaked = Box::leak(leaked);
		self.interned.insert(leaked);
		return leaked;
	}
	fn alloc_metadata<'t>(
		&'t mut self,
		target: &'static str,
		level: Level,
		kind: MetadataKind,
		name: &'static str,
		module: Option<&'static str>,
		file: Option<&'static str>,
		line: Option<u32>,
		names: &'static [&'static str],
	) -> &'static Metadata<'static> {
		let info = ForeignMetadataInfo {
			target,
			level,
			kind,
			name,
			module,
			file,
			line,
			names,
		};
		if let Some(v) = self.metadatas.get(&info) {
			return *v;
		}
		let fake = FakeCallsite;
		let metadata = Box::leak::<'static>(Box::new(Metadata::new(
			name,
			target,
			level,
			file,
			line,
			module,
			FieldSet::new(names, tracing::callsite::Identifier(&FAKE_CALLSITE)),
			kind.kind(),
		)));

		let meta_raw = &raw const *metadata;
		let fields_raw = &raw const *metadata.fields();

		// SAFETY: FieldSet struct should be inside of metadata struct... Which we assume here, but do not test
		// FIXME: Safety comment above might be invalidated at any time, this should actually be covered by unit test (or, better: runtime assertion... Somehow.)
		let fields_offset = unsafe { fields_raw.cast::<u8>().offset_from(meta_raw.cast()) };
		let field_set = unsafe {
			((&raw mut *metadata).cast::<()>())
				.byte_offset(fields_offset)
				.cast::<FieldSet>()
		};
		// FIXME: metadata borrow here invalidates our &mut borrow of 'static Metadata, and 'static FieldSet so this construction should be replaced with raw pointers or idk.
		// Something should be better done inside of tracing crate itself, someting like interior mutability.
		let callsite = Box::leak(Box::new(tracing::callsite::DefaultCallsite::new(metadata)));
		unsafe { *field_set = FieldSet::new(names, tracing::callsite::Identifier(callsite)) };

		tracing::callsite::register(&*callsite);

		self.metadatas.insert(info, metadata);
		return metadata;
	}
}

#[cfg(false)]
static FOREIGN_SPAN_DATA: LazyLock<Mutex<ForeignSpanData>> =
	LazyLock::new(|| Mutex::new(ForeignSpanData::default()));
static NIX_SPAN_MAPPING: LazyLock<Mutex<HashMap<u64, Span>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
enum FieldValue {
	Int(i32),
	Str(String),
}

struct StartActivityBuilder {
	activity_id: u64,
	verbosity: Verbosity,
	typ: ActivityType,
	fields: Vec<FieldValue>,
}
impl StartActivityBuilder {
	fn add_int_field(&mut self, i: i32) {
		self.fields.push(FieldValue::Int(i));
	}
	fn add_string_field(&mut self, v: &str) {
		self.fields.push(FieldValue::Str(v.to_owned()));
	}
	fn emit(&mut self, parent: u64, s: &str) {
		let mut mapping = NIX_SPAN_MAPPING.lock().expect("not poisoned");

		let parent = mapping.get(&parent);

		// let meta = spans.alloc_metadata(
		// 	self.typ.name(),
		// 	self.verbosity.into(),
		// 	MetadataKind::Span,
		// 	"nix activity start",
		// 	None,
		// 	None,
		// 	None,
		// 	self.typ.fields(),
		// );
		//
		// let mut fields = meta.fields().iter();
		// let span = if let Some(parent) = parent {
		// 	let s = Span::new(
		// 		meta,
		// 		&match meta.fields().len() {
		// 			1 => meta.fields().value_set(
		// 				&<[_; 1]>::try_from([(
		// 					&fields.next().expect("has field"),
		// 					Some(&format_args!("Test") as &dyn tracing::Value),
		// 				)])
		// 				.expect("valid size"),
		// 			),
		// 			_ => unreachable!(),
		// 		},
		// 	);
		// 	s.follows_from(parent);
		// 	s
		// } else {
		// 	Span::new_root(
		// 		meta,
		// 		&match meta.fields().len() {
		// 			1 => meta.fields().value_set(
		// 				&<[_; 1]>::try_from([(
		// 					&fields.next().expect("has field"),
		// 					Some(&format_args!("Test") as &dyn tracing::Value),
		// 				)])
		// 				.expect("valid size"),
		// 			),
		// 			_ => unreachable!(),
		// 		},
		// 	)
		// };
		//
		// let id = span.id().expect("id created");

		let span = {
			let _in_parent = parent.map(|p| p.enter());
			let level: Level = self.verbosity.into();
			if level == Level::ERROR {
				self.typ
					.format(&self.fields, s, |v| error_span!("action", v))
			} else if level == Level::WARN {
				self.typ
					.format(&self.fields, s, |v| warn_span!("action", v))
			} else if level == Level::INFO {
				self.typ
					.format(&self.fields, s, |v| info_span!("action", v))
			} else if level == Level::DEBUG {
				self.typ
					.format(&self.fields, s, |v| debug_span!("action", v))
			} else {
				self.typ
					.format(&self.fields, s, |v| trace_span!("action", v))
			}
		};
		if !s.trim().is_empty() {
			span.pb_set_message(s);
			let _e = span.enter();
			let level: Level = self.verbosity.into();
			if level == Level::ERROR {
				error!(target: "nix", "{}", s)
			} else if level == Level::WARN {
				warn!(target: "nix", "{}", s)
			} else if level == Level::INFO {
				info!(target: "nix", "{}", s)
			} else if level == Level::DEBUG {
				debug!(target: "nix", "{}", s)
			} else {
				trace!(target: "nix", "{}", s)
			}
		} else {
			span.pb_start();
		}
		mapping.insert(self.activity_id, span);
	}
	fn emit_result(&mut self, ty: u32) {
		let mut mapping = NIX_SPAN_MAPPING.lock().expect("not poisoned");

		let Some(parent) = mapping.get(&self.activity_id) else {
			panic!("unexpected result for dead parent");
		};

		let _in_parent = parent.enter();
		let res = ResultType::from_int(ty);

		use FieldValue::*;
		match (&res, self.fields.as_slice()) {
			// ResultType::FileLinked => todo!(),
			(ResultType::BuildLogLine, [Str(s)]) => {
				info!("{s:?}");
			}
			// ResultType::UntrustedPath => todo!(),
			// ResultType::CorruptedPath => todo!(),
			// ResultType::SetPhase => todo!(),
			(ResultType::SetExpected, [Int(act_ty), Int(_expected)]) => {
				let _act_ty = ActivityType::from_int(*act_ty as u32);
			}
			(ResultType::SetPhase, [Str(phase)]) => {
				// parent.pb_set_message(phase);
				debug!(target: "nix::phase", phase)
			}
			(ResultType::Progress, [Int(done), Int(expected), Int(_), Int(_)]) => {
				parent.pb_set_length(*expected as u64);
				parent.pb_set_position(*done as u64);
			}
			_ => warn!("unknown progress report: {:?}({:?})", &res, &self.fields),
		}
	}
}
fn new_start_activity(activity_id: u64, lvl: u32, typ: u32) -> Box<StartActivityBuilder> {
	Box::new(StartActivityBuilder {
		activity_id,
		verbosity: Verbosity::from_int(lvl),
		typ: ActivityType::from_int(typ),
		fields: vec![],
	})
}

fn emit_warn(v: &str) {
	warn!(target: "nix::eval", "{v}")
}
fn emit_stop(v: u64) {
	let mut mapping = NIX_SPAN_MAPPING.lock().expect("not poisoned");
	mapping.remove(&v);
}
fn emit_log(lvl: u32, v: &str) {
	let verbosity = Verbosity::from_int(lvl);
	let level: Level = verbosity.into();
	if level == Level::ERROR {
		error!(target: "nix", "{v}")
	} else if level == Level::WARN {
		warn!(target: "nix", "{v}")
	} else if level == Level::INFO {
		info!(target: "nix", "{v}")
	} else if level == Level::DEBUG {
		debug!(target: "nix", "{v}")
	} else {
		trace!(target: "nix", "{v}")
	}
}

// fn start_activity(act: u64, lvl: u32, act_ty: u32, s: &str, parent: u32) {
// 	tracing::Span::new(meta, values)
// }

#[cxx::bridge]
pub mod nix_logging_cxx {
	extern "Rust" {
		type StartActivityBuilder;
		fn new_start_activity(activity_id: u64, lvl: u32, typ: u32) -> Box<StartActivityBuilder>;
		fn add_int_field(&mut self, i: i32);
		fn add_string_field(&mut self, v: &str);
		fn emit(&mut self, parent: u64, s: &str);
		fn emit_result(&mut self, ty: u32);

		fn emit_warn(v: &str);
		fn emit_stop(id: u64);
		fn emit_log(lvl: u32, v: &str);
	}
	unsafe extern "C++" {
		include!("nix-eval/src/logging.hh");

		fn apply_tracing_logger();
	}
}
