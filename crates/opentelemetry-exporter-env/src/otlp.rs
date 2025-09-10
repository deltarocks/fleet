use std::collections::HashMap;
use std::time::Duration;

use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::{
	LogExporter, MetricExporter, SpanExporter, WithExportConfig as _, WithHttpConfig as _,
	WithTonicConfig as _,
};

use crate::{
	OtlpBaseSettings, OtlpLogsSettings, OtlpMetricsSettings, OtlpProtocol, ProviderError,
	ProviderResult,
};

fn parse_headers<'a>(
	headers: &'a str,
) -> std::iter::Map<std::str::Split<'a, char>, impl FnMut(&'a str) -> (&'a str, &'a str)> {
	headers.split(',').map(|header| {
		let mut parts = header.splitn(2, '=');
		let key = parts.next().unwrap();
		let value = parts.next().unwrap_or("");
		(key, value)
	})
}

fn parse_headers_metadata_map(headers: Option<&str>) -> MetadataMap {
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
fn parse_headers_hashmap(headers: Option<&str>) -> HashMap<String, String> {
	headers
		.map(|headers| {
			parse_headers(headers)
				.map(|(key, value)| (key.into(), value.into()))
				.collect()
		})
		.unwrap_or_default()
}

fn logger_exporter(base: &OtlpBaseSettings, log: &OtlpLogsSettings) -> ProviderResult<LogExporter> {
	let endpoint = log
		.endpoint
		.clone()
		.or_else(|| Some(format!("{}/v1/logs", base.endpoint.as_ref()?)))
		.ok_or(ProviderError::EndpointUnset)?;
	let headers = log.headers.as_deref().or(base.headers.as_deref());
	let timeout = Duration::from_millis(log.timeout.or(base.timeout).unwrap_or(10000));

	let protocol = log
		.protocol
		.or(base.protocol)
		.ok_or(ProviderError::UnsetProtocol)?;

	match protocol {
		OtlpProtocol::Grpc => {
			let mut builder = LogExporter::builder()
				.with_tonic()
				.with_endpoint(endpoint)
				.with_metadata(parse_headers_metadata_map(headers))
				.with_protocol(protocol.into())
				.with_timeout(timeout);
			let compression = log.compression.or(base.compression);
			if let Some(compression) = compression {
				builder = builder.with_compression(compression.into());
			}

			Ok(builder.build()?)
		}
		OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
			let builder = LogExporter::builder()
				.with_http()
				.with_endpoint(endpoint)
				.with_headers(parse_headers_hashmap(headers))
				.with_protocol(protocol.into())
				.with_timeout(timeout);

			Ok(builder.build()?)
		}
	}
}
fn metric_exporter(
	base: &OtlpBaseSettings,
	metric: &OtlpMetricsSettings,
) -> ProviderResult<MetricExporter> {
	let endpoint = metric
		.endpoint
		.clone()
		.or_else(|| Some(format!("{}/v1/metrics", base.endpoint.as_ref()?)))
		.ok_or(ProviderError::EndpointUnset)?;
	let headers = metric.headers.as_deref().or(base.headers.as_deref());
	let timeout = Duration::from_millis(metric.timeout.or(base.timeout).unwrap_or(10000));

	let protocol = metric
		.protocol
		.or(base.protocol)
		.ok_or(ProviderError::UnsetProtocol)?;

	match protocol {
		OtlpProtocol::Grpc => {
			let mut builder = MetricExporter::builder()
				.with_tonic()
				.with_endpoint(endpoint)
				.with_metadata(parse_headers_metadata_map(headers))
				.with_protocol(protocol.into())
				.with_timeout(timeout);
			let compression = metric.compression.or(base.compression);
			if let Some(compression) = compression {
				builder = builder.with_compression(compression.into());
			}

			Ok(builder.build()?)
		}
		OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
			let builder = MetricExporter::builder()
				.with_http()
				.with_endpoint(endpoint)
				.with_headers(parse_headers_hashmap(headers))
				.with_protocol(protocol.into())
				.with_timeout(timeout);

			Ok(builder.build()?)
		}
	}
}
fn span_exporter(
	base: &OtlpBaseSettings,
	trace: &OtlpMetricsSettings,
) -> ProviderResult<SpanExporter> {
	let endpoint = trace
		.endpoint
		.clone()
		.or_else(|| Some(format!("{}/v1/traces", base.endpoint.as_ref()?)))
		.ok_or(ProviderError::EndpointUnset)?;
	let headers = trace.headers.as_deref().or(base.headers.as_deref());
	let timeout = Duration::from_millis(trace.timeout.or(base.timeout).unwrap_or(10000));

	let protocol = trace
		.protocol
		.or(base.protocol)
		.ok_or(ProviderError::UnsetProtocol)?;

	match protocol {
		OtlpProtocol::Grpc => {
			let mut builder = SpanExporter::builder()
				.with_tonic()
				.with_endpoint(endpoint)
				.with_metadata(parse_headers_metadata_map(headers))
				.with_protocol(protocol.into())
				.with_timeout(timeout);
			let compression = trace.compression.or(base.compression);
			if let Some(compression) = compression {
				builder = builder.with_compression(compression.into());
			}

			Ok(builder.build()?)
		}
		OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
			let builder = SpanExporter::builder()
				.with_http()
				.with_endpoint(endpoint)
				.with_headers(parse_headers_hashmap(headers))
				.with_protocol(protocol.into())
				.with_timeout(timeout);

			Ok(builder.build()?)
		}
	}
}
