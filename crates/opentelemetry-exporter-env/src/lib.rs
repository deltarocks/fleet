use std::collections::HashMap;
use std::convert::Infallible;
use std::env::{self, VarError};
use std::ffi::OsString;
use std::num::ParseIntError;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
#[cfg(feature = "otlp")]
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
#[cfg(feature = "otlp")]
use opentelemetry_otlp::{
	ExporterBuildError, LogExporter, MetricExporter, SpanExporter, WithExportConfig,
	WithHttpConfig, WithTonicConfig,
};

#[cfg(feature = "otlp")]
mod otlp;

pub enum Error {
	InvalidUtf8 {
		env: &'static str,
		value: OsString,
	},
	EnvParseError {
		env: &'static str,
		value: String,
		error: &'static str,
	},
	EnvParseIntError {
		env: &'static str,
		value: String,
		error: ParseIntError,
	},
}
impl From<(&'static str, &'static str, String)> for Error {
	fn from((env, error, value): (&'static str, &'static str, String)) -> Self {
		Self::EnvParseError { env, value, error }
	}
}
impl From<(&'static str, ParseIntError, String)> for Error {
	fn from((env, error, value): (&'static str, ParseIntError, String)) -> Self {
		Self::EnvParseIntError { env, value, error }
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
		#[derive(Parser)]
		pub struct $id {
			$(
				$(#[doc = $doc])*
				#[arg(long = concat!("otel-exporter-otlp-", $long_prefix, $long), env = concat!("OTEL_EXPORTER_OTLP_", $env_prefix, $env), $($($tt)*)?)]
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
					_ => return Err("unsupported value, supported are")
				})
			}
		}
	};
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

impl_settings! {
	#[name("", "")]
	struct OtlpBaseSettings {
		/// Specifies the OTLP transport compression to be used for all telemetry data.
		#[name("COMPRESSION", "compression")]
		#[arg(value_enum)]
		compression: Compression,
		/// A base endpoint URL for any signal type, with an optionally-specified port number. Helpful for when you’re sending more than one signal to the same endpoint and want one environment variable to control the endpoint.
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

#[derive(thiserror::Error, Debug)]
enum ProviderError {
	#[error("protocol is not set")]
	UnsetProtocol,
	#[error("endpoint is not set")]
	EndpointUnset,
	#[cfg(feature = "otlp")]
	#[error("failed to build exporter: {0}")]
	Exporter(#[from] ExporterBuildError),
}
type ProviderResult<T, E = ProviderError> = Result<T, E>;
