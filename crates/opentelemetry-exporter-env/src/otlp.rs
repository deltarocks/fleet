use std::collections::HashMap;

use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::{
	LogExporter, MetricExporter, SpanExporter, WithExportConfig as _, WithHttpConfig as _,
	WithTonicConfig as _,
};

use crate::{Error, OtlpProtocol, ResolvedOtlpSettings};

fn parse_headers(headers: &str) -> impl Iterator<Item = (&str, &str)> {
	headers.split(',').map(|header| {
		let mut parts = header.splitn(2, '=');
		let key = parts.next().unwrap();
		let value = parts.next().unwrap_or("");
		(key, value)
	})
}

fn to_metadata_map(headers: Option<&str>) -> MetadataMap {
	headers
		.map(|headers| {
			MetadataMap::from_headers(
				parse_headers(headers)
					.map(|(key, value)| (key.parse().unwrap(), value.parse().unwrap()))
					.collect(),
			)
		})
		.unwrap_or_default()
}

fn to_hashmap(headers: Option<&str>) -> HashMap<String, String> {
	headers
		.map(|headers| {
			parse_headers(headers)
				.map(|(key, value)| (key.into(), value.into()))
				.collect()
		})
		.unwrap_or_default()
}

macro_rules! build_exporter {
	($exporter:ty, $settings:expr) => {{
		let s: &ResolvedOtlpSettings = $settings;
		match s.protocol {
			OtlpProtocol::Grpc => {
				let mut builder = <$exporter>::builder()
					.with_tonic()
					.with_endpoint(&s.endpoint)
					.with_metadata(to_metadata_map(s.headers.as_deref()))
					.with_protocol(s.protocol.into())
					.with_timeout(s.timeout);
				if let Some(compression) = s.compression {
					builder = builder.with_compression(compression.into());
				}
				builder.build()
			}
			OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
				<$exporter>::builder()
					.with_http()
					.with_endpoint(&s.endpoint)
					.with_headers(to_hashmap(s.headers.as_deref()))
					.with_protocol(s.protocol.into())
					.with_timeout(s.timeout)
					.build()
			}
		}
	}};
}

impl ResolvedOtlpSettings {
	pub fn span_exporter(&self) -> Result<SpanExporter, Error> {
		Ok(build_exporter!(SpanExporter, self)?)
	}

	pub fn log_exporter(&self) -> Result<LogExporter, Error> {
		Ok(build_exporter!(LogExporter, self)?)
	}

	pub fn metric_exporter(&self) -> Result<MetricExporter, Error> {
		Ok(build_exporter!(MetricExporter, self)?)
	}
}
