//
//! OpenTelemetry middleware.
//!
//! See [`opentelemetry_tracing_layer`] for more details.

use pin_axum::{
    extract::{ConnectInfo, MatchedPath, OriginalUri},
    response::Response,
};
use pin_http::{header, uri::Scheme, HeaderMap, Method, Request, Version};
use opentelemetry::trace::{TraceContextExt, TraceId};
use std::{borrow::Cow, net::SocketAddr, time::Duration};
use pin_tower_http::{
    classify::{
        GrpcErrorsAsFailures, GrpcFailureClass, ServerErrorsAsFailures, ServerErrorsFailureClass,
        SharedClassifier,
    },
    trace::{MakeSpan, OnBodyChunk, OnEos, OnFailure, OnRequest, OnResponse, TraceLayer},
};
use tracing::{field::Empty, Span};

/// OpenTelemetry tracing middleware.
///
/// This returns a [`TraceLayer`] configured to use [OpenTelemetry's conventional span field
/// names][otel].
///
/// # Span fields
///
/// The following fields will be set on the span:
///
/// - `http.client_ip`: The client's IP address. Requires using
/// [`Router::into_make_service_with_connect_info`]
/// - `http.flavor`: The protocol version used (http 1.1, http 2.0, etc)
/// - `http.host`: The value of the `Host` header
/// - `http.method`: The request method
/// - `http.route`: The matched route
/// - `http.scheme`: The URI scheme used (`HTTP` or `HTTPS`)
/// - `http.status_code`: The response status code
/// - `http.target`: The full request target including path and query parameters
/// - `http.user_agent`: The value of the `User-Agent` header
/// - `otel.kind`: Always `server`
/// - `otel.status_code`: `OK` if the response is success, `ERROR` if it is a 5xx
/// - `trace_id`: The trace id as tracted via the remote span context.
///
/// # Example
///
/// ```
/// use pin_axum::{Router, routing::get, http::Request};
/// use axum_tracing_opentelemetry::opentelemetry_tracing_layer;
/// use std::net::SocketAddr;
/// use pin_tower::ServiceBuilder;
///
/// let app = Router::new()
///     .route("/", get(|| async {}))
///     .layer(opentelemetry_tracing_layer());
///
/// # async {
/// pin_axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
///     // we must use `into_make_service_with_connect_info` for `opentelemetry_tracing_layer` to
///     // access the client ip
///     .serve(app.into_make_service_with_connect_info::<SocketAddr>())
///     .await
///     .expect("server failed");
/// # };
/// ```
///
/// # Complete example
///
/// See the "opentelemetry-jaeger" example for a complete setup that includes an OpenTelemetry
/// pipeline sending traces to jaeger.
///
/// [otel]: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/trace/semantic_conventions/http.md
/// [`Router::into_make_service_with_connect_info`]: axum::Router::into_make_service_with_connect_info
pub fn opentelemetry_tracing_layer() -> TraceLayer<
    SharedClassifier<ServerErrorsAsFailures>,
    OtelMakeSpan,
    OtelOnRequest,
    OtelOnResponse,
    OtelOnBodyChunk,
    OtelOnEos,
    OtelOnFailure,
> {
    TraceLayer::new_for_http()
        .make_span_with(OtelMakeSpan)
        .on_request(OtelOnRequest)
        .on_response(OtelOnResponse)
        .on_body_chunk(OtelOnBodyChunk)
        .on_eos(OtelOnEos)
        .on_failure(OtelOnFailure)
}

/// OpenTelemetry tracing middleware for gRPC.
pub fn opentelemetry_tracing_layer_grpc() -> TraceLayer<
    SharedClassifier<GrpcErrorsAsFailures>,
    OtelMakeGrpcSpan,
    OtelOnRequest,
    OtelOnResponse,
    OtelOnBodyChunk,
    OtelOnEos,
    OtelOnGrpcFailure,
> {
    TraceLayer::new_for_grpc()
        .make_span_with(OtelMakeGrpcSpan)
        .on_request(OtelOnRequest)
        .on_response(OtelOnResponse)
        .on_body_chunk(OtelOnBodyChunk)
        .on_eos(OtelOnEos)
        .on_failure(OtelOnGrpcFailure)
}

/// A [`MakeSpan`] that creates tracing spans using [OpenTelemetry's conventional field names][otel].
///
/// [otel]: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/trace/semantic_conventions/http.md
#[derive(Clone, Copy, Debug)]
pub struct OtelMakeSpan;

impl<B> MakeSpan<B> for OtelMakeSpan {
    fn make_span(&mut self, req: &Request<B>) -> Span {
        let user_agent = req
            .headers()
            .get(header::USER_AGENT)
            .map_or("", |h| h.to_str().unwrap_or(""));

        let host = req
            .headers()
            .get(header::HOST)
            .map_or("", |h| h.to_str().unwrap_or(""));

        let scheme = req
            .uri()
            .scheme()
            .map_or_else(|| "HTTP".into(), http_scheme);

        let http_route = req
            .extensions()
            .get::<MatchedPath>()
            .map_or_else(|| "", |mp| mp.as_str())
            .to_owned();

        let uri = if let Some(uri) = req.extensions().get::<OriginalUri>() {
            uri.0.clone()
        } else {
            req.uri().clone()
        };
        let http_target = uri
            .path_and_query()
            .map(|path_and_query| path_and_query.to_string())
            .unwrap_or_else(|| uri.path().to_owned());

        let client_ip = parse_x_forwarded_for(req.headers())
            .or_else(|| {
                req.extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(client_ip)| Cow::from(client_ip.to_string()))
            })
            .unwrap_or_default();
        let http_method_v = http_method(req.method());
        let name = format!("{http_method_v} {http_route}").trim().to_string();
        let (trace_id, otel_context) =
            create_context_with_trace(extract_remote_context(req.headers()));
        let span = tracing::info_span!(
            "HTTP request",
            otel.name= %name,
            http.client_ip = %client_ip,
            http.flavor = %http_flavor(req.version()),
            http.host = %host,
            http.method = %http_method_v,
            http.route = %http_route,
            http.scheme = %scheme,
            http.status_code = Empty,
            http.target = %http_target,
            http.user_agent = %user_agent,
            otel.kind = %"server", //opentelemetry::trace::SpanKind::Server
            otel.status_code = Empty,
            trace_id = %trace_id,
        );
        match otel_context {
            OtelContext::Remote(cx) => {
                tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(&span, cx)
            }
            OtelContext::Local(cx) => {
                tracing_opentelemetry::OpenTelemetrySpanExt::add_link(&span, cx)
            }
        }
        span
    }
}

/// A [`MakeSpan`] that creates tracing spans using [OpenTelemetry's conventional field names][otel] for gRPC services.
///
/// [otel]: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/trace/semantic_conventions/http.md
#[derive(Clone, Copy, Debug)]
pub struct OtelMakeGrpcSpan;

impl<B> MakeSpan<B> for OtelMakeGrpcSpan {
    fn make_span(&mut self, req: &Request<B>) -> Span {
        let user_agent = req
            .headers()
            .get(header::USER_AGENT)
            .map_or("", |h| h.to_str().unwrap_or(""));

        let host = req
            .headers()
            .get(header::HOST)
            .map_or("", |h| h.to_str().unwrap_or(""));

        let scheme = req
            .uri()
            .scheme()
            .map_or_else(|| "HTTP".into(), http_scheme);

        let http_route = req
            .extensions()
            .get::<MatchedPath>()
            .map_or("", |mp| mp.as_str())
            .to_owned();

        let uri = if let Some(uri) = req.extensions().get::<OriginalUri>() {
            uri.0.clone()
        } else {
            req.uri().clone()
        };
        let http_target = uri
            .path_and_query()
            .map(|path_and_query| path_and_query.to_string())
            .unwrap_or_else(|| uri.path().to_owned());

        let client_ip = parse_x_forwarded_for(req.headers())
            .or_else(|| {
                req.extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(client_ip)| Cow::from(client_ip.to_string()))
            })
            .unwrap_or_default();
        let http_method_v = http_method(req.method());
        let (trace_id, otel_context) =
            create_context_with_trace(extract_remote_context(req.headers()));
        let span = tracing::info_span!(
            "grpc request",
            otel.name = %http_target, // Convetion in gRPC tracing.
            http.client_ip = %client_ip,
            http.flavor = %http_flavor(req.version()),
            http.grpc_status = Empty,
            http.host = %host,
            http.method = %http_method_v,
            http.route = %http_route,
            http.scheme = %scheme,
            http.status_code = Empty,
            http.target = %http_target,
            http.user_agent = %user_agent,
            otel.kind = %"server", //opentelemetry::trace::SpanKind::Server
            otel.status_code = Empty,
            trace_id = %trace_id,
        );
        match otel_context {
            OtelContext::Remote(cx) => {
                tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(&span, cx)
            }
            OtelContext::Local(cx) => {
                tracing_opentelemetry::OpenTelemetrySpanExt::add_link(&span, cx)
            }
        }
        span
    }
}

fn parse_x_forwarded_for(headers: &HeaderMap) -> Option<Cow<'_, str>> {
    let value = headers.get("x-forwarded-for")?;
    let value = value.to_str().ok()?;
    let mut ips = value.split(',');
    Some(ips.next()?.trim().into())
}

fn http_method(method: &Method) -> Cow<'static, str> {
    match method {
        &Method::CONNECT => "CONNECT".into(),
        &Method::DELETE => "DELETE".into(),
        &Method::GET => "GET".into(),
        &Method::HEAD => "HEAD".into(),
        &Method::OPTIONS => "OPTIONS".into(),
        &Method::PATCH => "PATCH".into(),
        &Method::POST => "POST".into(),
        &Method::PUT => "PUT".into(),
        &Method::TRACE => "TRACE".into(),
        other => other.to_string().into(),
    }
}

fn http_flavor(version: Version) -> Cow<'static, str> {
    match version {
        Version::HTTP_09 => "0.9".into(),
        Version::HTTP_10 => "1.0".into(),
        Version::HTTP_11 => "1.1".into(),
        Version::HTTP_2 => "2.0".into(),
        Version::HTTP_3 => "3.0".into(),
        other => format!("{other:?}").into(),
    }
}

fn http_scheme(scheme: &Scheme) -> Cow<'static, str> {
    if scheme == &Scheme::HTTP {
        "http".into()
    } else if scheme == &Scheme::HTTPS {
        "https".into()
    } else {
        scheme.to_string().into()
    }
}

// If remote request has no span data the propagator defaults to an unsampled context
fn extract_remote_context(headers: &HeaderMap) -> opentelemetry::Context {
    struct HeaderExtractor<'a>(&'a HeaderMap);

    impl<'a> opentelemetry::propagation::Extractor for HeaderExtractor<'a> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|value| value.to_str().ok())
        }

        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(|value| value.as_str()).collect()
        }
    }
    let extractor = HeaderExtractor(headers);
    opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(&extractor))
}

enum OtelContext {
    Remote(opentelemetry::Context),
    Local(opentelemetry::trace::SpanContext),
}

//HACK create a context with a trace_id (if not set) before call to
// `tracing_opentelemetry::OpenTelemetrySpanExt::set_parent`
// else trace_id is defined too late and the `info_span` log `trace_id: ""`
// Use the default global tracer (named "") to start the trace
fn create_context_with_trace(remote_context: opentelemetry::Context) -> (TraceId, OtelContext) {
    if !remote_context.span().span_context().is_valid() {
        // create a fake remote context but with a fresh new trace_id
        use opentelemetry_sdk::trace::IdGenerator;
        use opentelemetry_sdk::trace::RandomIdGenerator;
        use opentelemetry::trace::SpanContext;
        let trace_id = RandomIdGenerator::default().new_trace_id();
        let span_id = RandomIdGenerator::default().new_span_id();
        let new_span_context = SpanContext::new(
            trace_id,
            span_id,
            remote_context.span().span_context().trace_flags(),
            false,
            remote_context.span().span_context().trace_state().clone(),
        );
        (trace_id, OtelContext::Local(new_span_context))
    } else {
        let remote_span = remote_context.span();
        let span_context = remote_span.span_context();
        let trace_id = span_context.trace_id();
        (trace_id, OtelContext::Remote(remote_context))
    }
}

/// Callback that [`Trace`] will call when it receives a request.
///
/// [`Trace`]: tower_http::trace::Trace
#[derive(Clone, Copy, Debug)]
pub struct OtelOnRequest;

impl<B> OnRequest<B> for OtelOnRequest {
    #[inline]
    fn on_request(&mut self, _request: &Request<B>, _span: &Span) {}
}

/// Callback that [`Trace`] will call when it receives a response.
///
/// [`Trace`]: tower_http::trace::Trace
#[derive(Clone, Copy, Debug)]
pub struct OtelOnResponse;

impl<B> OnResponse<B> for OtelOnResponse {
    fn on_response(self, response: &Response<B>, _latency: Duration, span: &Span) {
        let status = response.status().as_u16().to_string();
        span.record("http.status_code", &tracing::field::display(status));

        // assume there is no error, if there is `OtelOnFailure` will be called and override this
        span.record("otel.status_code", "OK");
    }
}

/// Callback that [`Trace`] will call when the response body produces a chunk.
///
/// [`Trace`]: tower_http::trace::Trace
#[derive(Clone, Copy, Debug)]
pub struct OtelOnBodyChunk;

impl<B> OnBodyChunk<B> for OtelOnBodyChunk {
    #[inline]
    fn on_body_chunk(&mut self, _chunk: &B, _latency: Duration, _span: &Span) {}
}

/// Callback that [`Trace`] will call when a streaming response completes.
///
/// [`Trace`]: tower_http::trace::Trace
#[derive(Clone, Copy, Debug)]
pub struct OtelOnEos;

impl OnEos for OtelOnEos {
    #[inline]
    fn on_eos(self, _trailers: Option<&HeaderMap>, _stream_duration: Duration, _span: &Span) {
    }
}

/// Callback that [`Trace`] will call when a response or end-of-stream is classified as a failure.
///
/// [`Trace`]: tower_http::trace::Trace
#[derive(Clone, Copy, Debug)]
pub struct OtelOnFailure;

impl OnFailure<ServerErrorsFailureClass> for OtelOnFailure {
    fn on_failure(&mut self, failure: ServerErrorsFailureClass, _latency: Duration, span: &Span) {
        match failure {
            ServerErrorsFailureClass::StatusCode(status) => {
                if status.is_server_error() {
                    span.record("otel.status_code", "ERROR");
                }
            }
            ServerErrorsFailureClass::Error(_) => {
                span.record("otel.status_code", "ERROR");
            }
        }
    }
}

/// Callback that [`Trace`] will call when a response or end-of-stream is classified as a failure.
///
/// [`Trace`]: tower_http::trace::Trace
#[derive(Clone, Copy, Debug)]
pub struct OtelOnGrpcFailure;

impl OnFailure<GrpcFailureClass> for OtelOnGrpcFailure {
    fn on_failure(&mut self, failure: GrpcFailureClass, _latency: Duration, span: &Span) {
        match failure {
            GrpcFailureClass::Code(code) => {
                span.record("http.grpc_status", code);
            }
            GrpcFailureClass::Error(_) => {
                span.record("http.grpc_status", 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::*;
    use pin_axum::{
        body::Body,
        http::StatusCode,
        handler::Handler,
        routing::{get, post},
        Router,
    };
    use pin_http::Request;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use rstest::*;
    use serde_json::Value;
    use std::sync::mpsc::{self, Receiver, SyncSender};

    use tracing_subscriber::{
        fmt::{format::FmtSpan, MakeWriter},
        util::SubscriberInitExt,
        EnvFilter,
    };

    #[rstest]
    #[case("filled_http_route_for_existing_route", "/users/123", &[], 0, false)]
    #[case("empty_http_route_for_nonexisting_route", "/idontexist/123", &[], 0, false)]
    #[case("status_code_on_close_for_ok", "/users/123", &[], 1, false)]
    #[case("status_code_on_close_for_error", "/status/500", &[], 1, false)]
    #[case("filled_http_headers", "/users/123", &[("user-agent", "tests"), ("x-forwarded-for", "127.0.0.1")], 0, false)]
    #[case("call_with_w3c_trace", "/users/123", &[("traceparent", "00-b2611246a58fd7ea623d2264c5a1e226-b2c9b811f2f424af-01")], 0, true)]
    #[case("trace_id_in_child_span", "/with_child_span", &[], 1, false)]
    #[case("trace_id_in_child_span_for_remote", "/with_child_span", &[("traceparent", "00-b2611246a58fd7ea623d2264c5a1e226-b2c9b811f2f424af-01")], 1, true)]
    // failed to extract "http.route" before axum-0.6.15
    // - https://github.com/davidB/axum-tracing-opentelemetry/pull/54 (reverted)
    // - https://github.com/tokio-rs/axum/issues/1441#issuecomment-1272158039
    #[case("extract_route_from_nested", "/nest/123", &[], 0, false)]
    #[tokio::test]
    async fn check_span_event(
        #[case] name: &str,
        #[case] uri: &str,
        #[case] headers: &[(&str, &str)],
        #[case] event_idx: usize,
        #[case] is_trace_id_constant: bool,
    ) {
        let svc = Router::new()
            .route("/users/:id", get(|| async { StatusCode::OK }))
            .route(
                "/status/500",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/with_child_span",
                get(|| async {
                    let span = tracing::span!(tracing::Level::INFO, "my child span");
                    span.in_scope(|| {
                        // Any trace events in this closure or code called by it will occur within
                        // the span.
                    });
                    StatusCode::OK
                }),
            )
            .nest(
                "/nest",
                Router::new()
                    .route("/:nest_id", get(|| async {}))
                    .fallback((|| async { (StatusCode::NOT_FOUND, "inner fallback") }).into_service()),
            )
            .fallback((|| async { (StatusCode::NOT_FOUND, "outer fallback") }).into_service())
            .layer(opentelemetry_tracing_layer());
        let mut builder = Request::builder();
        for (key, value) in headers.iter() {
            builder = builder.header(*key, *value);
        }
        let req = builder.uri(uri).body(Body::empty()).unwrap();
        let events = span_event_for_request(svc, req).await;
        insta::assert_yaml_snapshot!(name, events[event_idx], {
            ".timestamp" => "[timestamp]",
            ".fields[\"time.busy\"]" => "[duration]",
            ".fields[\"time.idle\"]" => "[duration]",
            ".span.trace_id" => insta::dynamic_redaction(move |value, _path| {
                let_assert!(Some(trace_id) = value.as_str());
                if is_trace_id_constant {
                    trace_id.to_string()
                } else {
                    format!("[trace_id:lg{}]", trace_id.len())
                }
            }),
            ".spans[0].trace_id" => insta::dynamic_redaction(move |value, _path| {
                let_assert!(Some(trace_id) = value.as_str());
                if is_trace_id_constant {
                    trace_id.to_string()
                } else {
                    format!("[trace_id:lg{}]", trace_id.len())
                }
            }),
        });
    }

    #[rstest]
    #[case("grpc_status_code_on_close_for_ok", "/module.service/endpoint1", &[], 1)]
    #[tokio::test]
    async fn check_span_event_grpc(
        #[case] name: &str,
        #[case] uri: &str,
        #[case] headers: &[(&str, &str)],
        #[case] event_idx: usize,
    ) {
        let svc = Router::new()
            .route(
                "/module.service/endpoint1",
                post(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("grpc-status", 2)
                        .body(Body::empty())
                        .unwrap()
                }),
            )
            .layer(opentelemetry_tracing_layer_grpc());
        let mut builder = Request::builder();
        for (key, value) in headers.iter() {
            builder = builder.header(*key, *value);
        }
        builder = builder.method("POST");
        let req = builder.uri(uri).body(Body::empty()).unwrap();
        let events = span_event_for_request(svc, req).await;
        insta::assert_yaml_snapshot!(name, events[event_idx], {
            ".timestamp" => "[timestamp]",
            ".fields[\"time.busy\"]" => "[duration]",
            ".fields[\"time.idle\"]" => "[duration]",
            ".span.trace_id" => insta::dynamic_redaction(|value, _path| {
                let_assert!(Some(trace_id) = value.as_str());
                format!("[trace_id:lg{}]", trace_id.len())
            }),
        });
    }

    async fn span_event_for_request(mut router: Router, req: Request<Body>) -> Vec<Value> {
        use pin_axum::body::HttpBody as _;
        use pin_tower::{Service, ServiceExt};
        use opentelemetry::trace::TracerProvider;
        use tracing_subscriber::layer::SubscriberExt;

        let tracer_provider = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(opentelemetry_otlp::new_exporter().tonic())
            .install_batch(opentelemetry_sdk::runtime::Tokio)
            .unwrap();
        let tracer = tracer_provider.tracer("axum-tracing-opentelemetry");
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        let (make_writer, rx) = duplex_writer();
        let fmt_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(make_writer)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE);
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::try_new("axum_extra=trace,info").unwrap())
            .with(fmt_layer)
            .with(otel_layer);
        let _guard = subscriber.set_default();

        let mut res = router.ready().await.unwrap().call(req).await.unwrap();

        while res.data().await.is_some() {}
        res.trailers().await.unwrap();
        drop(res);

        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap())
            .collect::<Vec<_>>()
    }

    fn duplex_writer() -> (DuplexWriter, Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::sync_channel(1024);
        (DuplexWriter { tx }, rx)
    }

    #[derive(Clone)]
    struct DuplexWriter {
        tx: SyncSender<Vec<u8>>,
    }

    impl<'a> MakeWriter<'a> for DuplexWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl std::io::Write for DuplexWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.tx.send(buf.to_vec()).unwrap();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
