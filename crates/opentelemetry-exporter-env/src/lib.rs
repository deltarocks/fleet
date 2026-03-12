use std::convert::Infallible;
use std::env::{self, VarError};
use std::ffi::OsString;
use std::num::ParseIntError;
use std::str::FromStr;
use std::time::Duration;

#[cfg(feature = "otlp")]
mod otlp;

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("environment variable {env} contains invalid UTF-8: {value:?}")]
	InvalidUtf8 {
		env: &'static str,
		value: OsString,
	},
	#[error("environment variable {env}={value:?}: {error}")]
	EnvParse {
		env: &'static str,
		value: String,
		error: &'static str,
	},
	#[error("environment variable {env}={value:?}: {error}")]
	EnvParseInt {
		env: &'static str,
		value: String,
		error: ParseIntError,
	},
	#[cfg(feature = "otlp")]
	#[error("failed to build exporter: {0}")]
	Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
}

impl From<(&'static str, &'static str, String)> for Error {
	fn from((env, error, value): (&'static str, &'static str, String)) -> Self {
		Self::EnvParse { env, value, error }
	}
}
impl From<(&'static str, ParseIntError, String)> for Error {
	fn from((env, error, value): (&'static str, ParseIntError, String)) -> Self {
		Self::EnvParseInt { env, value, error }
	}
}
impl From<(&'static str, Infallible, String)> for Error {
	fn from(_v: (&'static str, Infallible, String)) -> Self {
		unreachable!()
	}
}

fn load_env<T>(env: &'static str) -> Result<Option<T>, Error>
where
	T: FromStr,
	Error: From<(&'static str, <T as FromStr>::Err, String)>,
{
	match env::var(env) {
		Ok(v) => Ok(Some(T::from_str(&v).map_err(|err| (env, err, v))?)),
		Err(VarError::NotPresent) => Ok(None),
		Err(VarError::NotUnicode(value)) => Err(Error::InvalidUtf8 { env, value }),
	}
}

macro_rules! impl_enum {
	(enum $id:ident {
		$(
			#[name = $value:literal]
			$var:ident,
		)*
	}) => {
		#[derive(Clone, Copy)]
		#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
		pub enum $id {
			$(
				#[cfg_attr(feature = "clap", value(name = $value))]
				$var,
			)*
		}
		impl FromStr for $id {
			type Err = &'static str;

			fn from_str(s: &str) -> Result<Self, Self::Err> {
				Ok(match s {
					$(
						$value => Self::$var,
					)*
					_ => return Err("unsupported value")
				})
			}
		}
	};
}

impl_enum! {
	enum ExporterKind {
		#[name = "otlp"]
		Otlp,
		#[name = "none"]
		None,
	}
}

#[derive(Default)]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
pub struct SignalExporterSettings {
	/// Traces exporter to be used.
	#[cfg_attr(feature = "clap", arg(long = "otel-traces-exporter", env = "OTEL_TRACES_EXPORTER", value_enum))]
	pub traces: Option<ExporterKind>,
	/// Metrics exporter to be used.
	#[cfg_attr(feature = "clap", arg(long = "otel-metrics-exporter", env = "OTEL_METRICS_EXPORTER", value_enum))]
	pub metrics: Option<ExporterKind>,
	/// Logs exporter to be used.
	#[cfg_attr(feature = "clap", arg(long = "otel-logs-exporter", env = "OTEL_LOGS_EXPORTER", value_enum))]
	pub logs: Option<ExporterKind>,
}

impl SignalExporterSettings {
	pub fn from_env() -> Result<Self, Error> {
		Ok(Self {
			traces: load_env("OTEL_TRACES_EXPORTER")?,
			metrics: load_env("OTEL_METRICS_EXPORTER")?,
			logs: load_env("OTEL_LOGS_EXPORTER")?,
		})
	}

	pub fn traces_enabled(&self) -> bool {
		!matches!(self.traces, Some(ExporterKind::None))
	}
	pub fn metrics_enabled(&self) -> bool {
		!matches!(self.metrics, Some(ExporterKind::None))
	}
	pub fn logs_enabled(&self) -> bool {
		!matches!(self.logs, Some(ExporterKind::None))
	}
}

impl_enum! {
	enum Compression {
		#[name = "gzip"]
		Gzip,
		#[name = "zstd"]
		Zstd,
	}
}
#[cfg(feature = "otlp")]
impl From<Compression> for opentelemetry_otlp::Compression {
	fn from(value: Compression) -> Self {
		match value {
			Compression::Gzip => opentelemetry_otlp::Compression::Gzip,
			Compression::Zstd => opentelemetry_otlp::Compression::Zstd,
		}
	}
}

impl_enum! {
	enum OtlpProtocol {
		#[name = "grpc"]
		Grpc,
		#[name = "http/protobuf"]
		HttpProtobuf,
		#[name = "http/json"]
		HttpJson,
	}
}
#[cfg(feature = "otlp")]
impl From<OtlpProtocol> for opentelemetry_otlp::Protocol {
	fn from(value: OtlpProtocol) -> Self {
		match value {
			OtlpProtocol::Grpc => opentelemetry_otlp::Protocol::Grpc,
			OtlpProtocol::HttpProtobuf => opentelemetry_otlp::Protocol::HttpBinary,
			OtlpProtocol::HttpJson => opentelemetry_otlp::Protocol::HttpJson,
		}
	}
}

pub trait OtlpSignalSettings {
	fn compression(&self) -> Option<Compression>;
	fn endpoint(&self) -> Option<&str>;
	fn headers(&self) -> Option<&str>;
	fn protocol(&self) -> Option<OtlpProtocol>;
	fn timeout(&self) -> Option<u64>;
}

macro_rules! impl_settings {
	(
	#[name($env_prefix:literal, $long_prefix:literal)]
	struct $id:ident {
		$(
			$(#[doc = $doc:literal])*
			#[name($env:literal, $long:literal)]
			$(#[arg($($tt:tt)*)])?
			$name:ident: $ty:ty,
		)*
	}) => {
		#[derive(Default)]
		#[cfg_attr(feature = "clap", derive(clap::Parser))]
		pub struct $id {
			$(
				$(#[doc = $doc])*
				#[cfg_attr(feature = "clap", arg(long = concat!("otel-exporter-otlp-", $long_prefix, $long), env = concat!("OTEL_EXPORTER_OTLP_", $env_prefix, $env) $(, $($tt)*)?))]
				pub $name: Option<$ty>,
			)*
		}
		impl $id {
			pub fn from_env() -> Result<Self, Error> {
				Ok(Self {
					$(
						$name: load_env(concat!("OTEL_EXPORTER_OTLP_", $env_prefix, $env))?,
					)*
				})
			}
		}
		impl OtlpSignalSettings for $id {
			fn compression(&self) -> Option<Compression> { self.compression }
			fn endpoint(&self) -> Option<&str> { self.endpoint.as_deref() }
			fn headers(&self) -> Option<&str> { self.headers.as_deref() }
			fn protocol(&self) -> Option<OtlpProtocol> { self.protocol }
			fn timeout(&self) -> Option<u64> { self.timeout }
		}
	}
}

impl_settings! {
	#[name("", "")]
	struct OtlpBaseSettings {
		/// Specifies the OTLP transport compression to be used for all telemetry data.
		#[name("COMPRESSION", "compression")]
		#[arg(value_enum)]
		compression: Compression,
		/// A base endpoint URL for any signal type, with an optionally-specified port number. Helpful for when you're sending more than one signal to the same endpoint and want one environment variable to control the endpoint.
		#[name("ENDPOINT", "endpoint")]
		endpoint: String,
		/// A list of headers to apply to all outgoing data (traces, metrics, and logs).
		#[name("HEADERS", "headers")]
		headers: String,
		/// Specifies the OTLP transport protocol to be used for all telemetry data.
		#[name("PROTOCOL", "protocol")]
		#[arg(value_enum)]
		protocol: OtlpProtocol,
		/// The timeout value for all outgoing data (traces, metrics, and logs) in milliseconds.
		#[name("TIMEOUT", "timeout")]
		timeout: u64,
	}
}
impl_settings! {
	#[name("LOGS_", "logs-")]
	struct OtlpLogsSettings {
		/// Specifies the OTLP transport compression to be used for log data.
		#[name("COMPRESSION", "compression")]
		#[arg(value_enum)]
		compression: Compression,
		/// Endpoint URL for log data only, with an optionally-specified port number. Typically ends with `v1/logs` when using OTLP/HTTP.
		#[name("ENDPOINT", "endpoint")]
		endpoint: String,
		/// A list of headers to apply to all outgoing logs.
		#[name("HEADERS", "headers")]
		headers: String,
		/// Specifies the OTLP transport protocol to be used for log data.
		#[name("PROTOCOL", "protocol")]
		#[arg(value_enum)]
		protocol: OtlpProtocol,
		/// The timeout value for all outgoing logs in milliseconds.
		#[name("TIMEOUT", "timeout")]
		timeout: u64,
	}
}
impl_settings! {
	#[name("METRICS_", "metrics-")]
	struct OtlpMetricsSettings {
		/// Specifies the OTLP transport compression to be used for metrics data.
		#[name("COMPRESSION", "compression")]
		#[arg(value_enum)]
		compression: Compression,
		/// Endpoint URL for metric data only, with an optionally-specified port number. Typically ends with `v1/metrics` when using OTLP/HTTP.
		#[name("ENDPOINT", "endpoint")]
		endpoint: String,
		/// A list of headers to apply to all outgoing metrics.
		#[name("HEADERS", "headers")]
		headers: String,
		/// Specifies the OTLP transport protocol to be used for metrics data.
		#[name("PROTOCOL", "protocol")]
		#[arg(value_enum)]
		protocol: OtlpProtocol,
		/// The timeout value for all outgoing metrics in milliseconds.
		#[name("TIMEOUT", "timeout")]
		timeout: u64,
	}
}
impl_settings! {
	#[name("TRACES_", "traces-")]
	struct OtlpTracesSettings {
		/// Specifies the OTLP transport compression to be used for trace data.
		#[name("COMPRESSION", "compression")]
		#[arg(value_enum)]
		compression: Compression,
		/// Endpoint URL for trace data only, with an optionally-specified port number. Typically ends with `v1/traces` when using OTLP/HTTP.
		#[name("ENDPOINT", "endpoint")]
		endpoint: String,
		/// A list of headers to apply to all outgoing traces.
		#[name("HEADERS", "headers")]
		headers: String,
		/// Specifies the OTLP transport protocol to be used for trace data.
		#[name("PROTOCOL", "protocol")]
		#[arg(value_enum)]
		protocol: OtlpProtocol,
		/// The timeout value for all outgoing traces in milliseconds.
		#[name("TIMEOUT", "timeout")]
		timeout: u64,
	}
}

pub struct ResolvedOtlpSettings {
	pub compression: Option<Compression>,
	pub endpoint: String,
	pub headers: Option<String>,
	pub protocol: OtlpProtocol,
	pub timeout: Duration,
}

impl ResolvedOtlpSettings {
	const DEFAULT_TIMEOUT_MS: u64 = 10000;
	const DEFAULT_GRPC_ENDPOINT: &str = "http://localhost:4317";
	const DEFAULT_HTTP_ENDPOINT: &str = "http://localhost:4318";

	pub fn traces(
		base: &impl OtlpSignalSettings,
		signal: &impl OtlpSignalSettings,
	) -> Result<Self, Error> {
		Self::resolve(base, signal, "/v1/traces")
	}

	pub fn metrics(
		base: &impl OtlpSignalSettings,
		signal: &impl OtlpSignalSettings,
	) -> Result<Self, Error> {
		Self::resolve(base, signal, "/v1/metrics")
	}

	pub fn logs(
		base: &impl OtlpSignalSettings,
		signal: &impl OtlpSignalSettings,
	) -> Result<Self, Error> {
		Self::resolve(base, signal, "/v1/logs")
	}

	fn resolve(
		base: &impl OtlpSignalSettings,
		signal: &impl OtlpSignalSettings,
		signal_path: &str,
	) -> Result<Self, Error> {
		let protocol = signal
			.protocol()
			.or_else(|| base.protocol())
			.unwrap_or(OtlpProtocol::HttpProtobuf);

		let endpoint = if let Some(ep) = signal.endpoint() {
			ep.to_owned()
		} else if let Some(ep) = base.endpoint() {
			match protocol {
				OtlpProtocol::Grpc => ep.to_owned(),
				_ => format!("{ep}{signal_path}"),
			}
		} else {
			match protocol {
				OtlpProtocol::Grpc => Self::DEFAULT_GRPC_ENDPOINT.to_owned(),
				_ => format!("{}{signal_path}", Self::DEFAULT_HTTP_ENDPOINT),
			}
		};

		Ok(Self {
			compression: signal.compression().or_else(|| base.compression()),
			endpoint,
			headers: signal
				.headers()
				.or_else(|| base.headers())
				.map(str::to_owned),
			protocol,
			timeout: Duration::from_millis(
				signal
					.timeout()
					.or_else(|| base.timeout())
					.unwrap_or(Self::DEFAULT_TIMEOUT_MS),
			),
		})
	}
}
