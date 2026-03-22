//! Tracing initialisation supporting two modes:
//! - `Console`  – structured JSON to stdout (development / integration tests)
//! - `Otlp`     – JSON to stdout **plus** spans sent to a Jaeger/OTLP collector
//!
//! Read the mode from `OTEL_MODE` env var (`console` or `otlp`; default: `console`).
//! The OTLP endpoint is read from `OTEL_EXPORTER_OTLP_ENDPOINT`
//! (default: `http://localhost:4317`).
//!
//! OTLP export errors are logged but **never** crash the service.

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// How and where spans are exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracingMode {
    /// Structured JSON to stdout only.
    Console,
    /// JSON to stdout **and** OTLP gRPC to a Jaeger / OpenTelemetry collector.
    Otlp,
}

impl TracingMode {
    /// Read from `OTEL_MODE` env var; defaults to `Console`.
    pub fn from_env() -> Self {
        match std::env::var("OTEL_MODE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "otlp" => TracingMode::Otlp,
            _ => TracingMode::Console,
        }
    }
}

/// Initialise structured tracing.
///
/// Must be called **once** in `main()` before any instrumented code.
/// Safe to call multiple times (subsequent calls are silently ignored,
/// which is important for integration tests that spin up multiple services).
pub fn init_tracing(service_name: &str, mode: TracingMode) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let json_layer = fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true);

    match mode {
        TracingMode::Console => {
            let _ = tracing_subscriber::registry()
                .with(env_filter)
                .with(json_layer)
                .try_init();
        }

        TracingMode::Otlp => {
            let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:4317".to_string());

            let service_name_owned = service_name.to_owned();

            // Build the OTLP pipeline.  Errors here must NOT crash the service.
            let otlp_result = (|| -> Result<_, Box<dyn std::error::Error>> {
                use opentelemetry::KeyValue;
                use opentelemetry_otlp::WithExportConfig;
                use opentelemetry_sdk::{
                    propagation::TraceContextPropagator, runtime, trace as sdktrace,
                    Resource,
                };

                let tracer = opentelemetry_otlp::new_pipeline()
                    .tracing()
                    .with_exporter(
                        opentelemetry_otlp::new_exporter()
                            .tonic()
                            .with_endpoint(otlp_endpoint),
                    )
                    .with_trace_config(
                        sdktrace::config().with_resource(Resource::new(vec![
                            KeyValue::new("service.name", service_name_owned),
                        ])),
                    )
                    .install_batch(runtime::Tokio)?;

                // Install W3C TraceContext propagator so traceparent headers flow
                // through tonic metadata between services.
                opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

                Ok(tracer)
            })();

            match otlp_result {
                Ok(_tracer) => {
                    let otel_layer = tracing_opentelemetry::layer();
                    let _ = tracing_subscriber::registry()
                        .with(env_filter)
                        .with(json_layer)
                        .with(otel_layer)
                        .try_init();
                    tracing::info!(
                        service = %service_name,
                        mode = "otlp",
                        "tracing initialised"
                    );
                }
                Err(e) => {
                    // OTLP init failed — fall back to console so the service keeps running.
                    let _ = tracing_subscriber::registry()
                        .with(env_filter)
                        .with(json_layer)
                        .try_init();
                    tracing::error!(
                        error = %e,
                        service = %service_name,
                        "OTLP init failed — continuing with console tracing"
                    );
                }
            }
        }
    }
}
