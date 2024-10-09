use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{self as sdktrace, TracerProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::export::trace::{SpanData, SpanExporter, ExportResult};
use opentelemetry::{global, trace::{TraceError, TracerProvider as _}};
use std::fmt::Debug;

pub fn identity(v: sdktrace::Builder) -> sdktrace::Builder {
    v
}

pub fn init_tracer<F, E>(
    resource: Resource,
    transform: F,
    mut exporter: E,
) -> Result<sdktrace::Tracer, TraceError>
where
    F: FnOnce(sdktrace::Builder) -> sdktrace::Builder,
    E: SpanExporter + 'static,
{
    global::set_text_map_propagator(TraceContextPropagator::new());

    exporter.set_resource(&resource);
    let builder = TracerProvider::builder().with_simple_exporter(exporter);
    let provider = transform(builder).build();

    Ok(provider.tracer("axum-tracing-opentelemetry"))
}

#[derive(Debug, Default)]
pub enum StdoutExporter {
    #[default]
    Noop,
    Stdout {
        exporter: opentelemetry_stdout::SpanExporter,
    }
}

impl StdoutExporter {
    pub fn noop() -> Self {
        Self::Noop
    }

    pub fn new() -> Self {
        Self::Stdout {
            exporter: opentelemetry_stdout::SpanExporter::default(),
        }
    }
}

impl SpanExporter for StdoutExporter {
    fn export(&mut self, batch: Vec<SpanData>) -> futures::future::BoxFuture<'static, ExportResult> {
        match self {
            Self::Noop => Box::pin(futures::future::ready(Ok(()))),
            Self::Stdout { exporter } => exporter.export(batch),
        }
    }
}

