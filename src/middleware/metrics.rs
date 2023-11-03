//! # Metrics Middleware

use actix_http::header::CONTENT_LENGTH;
use actix_web::dev;
use futures_util::future::{self, FutureExt as _, LocalBoxFuture};
use opentelemetry_api::global;
use opentelemetry_api::metrics::{Histogram, Meter, MeterProvider, Unit, UpDownCounter};
use std::borrow::Cow;
use std::{sync::Arc, time::SystemTime};

use crate::util::metrics_attributes_from_request;
use crate::RouteFormatter;

// Follows the experimental semantic conventions for HTTP metrics:
// https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/metrics/semantic_conventions/http-metrics.md
use opentelemetry_semantic_conventions::trace::HTTP_RESPONSE_STATUS_CODE;
const HTTP_SERVER_DURATION: &str = "http.server.duration";
const HTTP_SERVER_ACTIVE_REQUESTS: &str = "http.server.active_requests";
const HTTP_SERVER_REQUEST_SIZE: &str = "http.server.request.size";
const HTTP_SERVER_RESPONSE_SIZE: &str = "http.server.response.size";

/// Records http server metrics
///
/// See the [spec] for details.
///
/// [spec]: https://github.com/open-telemetry/semantic-conventions/blob/v1.21.0/docs/http/http-metrics.md#http-server
#[derive(Clone, Debug)]
struct Metrics {
    http_server_duration: Histogram<f64>,
    http_server_active_requests: UpDownCounter<i64>,
    http_server_request_size: Histogram<u64>,
    http_server_response_size: Histogram<u64>,
}

impl Metrics {
    /// Create a new [`RequestMetrics`]
    fn new(meter: Meter) -> Self {
        let http_server_duration = meter
            .f64_histogram(HTTP_SERVER_DURATION)
            .with_description("Measures the duration of inbound HTTP requests.")
            .with_unit(Unit::new("s"))
            .init();

        let http_server_active_requests = meter
            .i64_up_down_counter(HTTP_SERVER_ACTIVE_REQUESTS)
            .with_description(
                "Measures the number of concurrent HTTP requests that are currently in-flight.",
            )
            .with_unit(Unit::new("{request}"))
            .init();

        let http_server_request_size = meter
            .u64_histogram(HTTP_SERVER_REQUEST_SIZE)
            .with_description("Measures the size of HTTP request messages (compressed).")
            .with_unit(Unit::new("By"))
            .init();

        let http_server_response_size = meter
            .u64_histogram(HTTP_SERVER_RESPONSE_SIZE)
            .with_description("Measures the size of HTTP request messages (compressed).")
            .with_unit(Unit::new("By"))
            .init();

        Metrics {
            http_server_active_requests,
            http_server_duration,
            http_server_request_size,
            http_server_response_size,
        }
    }
}

/// Builder for [RequestMetrics]
#[derive(Clone, Debug, Default)]
pub struct RequestMetricsBuilder {
    route_formatter: Option<Arc<dyn RouteFormatter + Send + Sync + 'static>>,
    meter: Option<Meter>,
    use_paths_in_route_label: bool,
}

impl RequestMetricsBuilder {
    /// Create a new `RequestMetricsBuilder`
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a route formatter to customize metrics match patterns
    pub fn with_route_formatter<R>(mut self, route_formatter: R) -> Self
    where
        R: RouteFormatter + Send + Sync + 'static,
    {
        self.route_formatter = Some(Arc::new(route_formatter));
        self
    }

    /// Set the meter provider this middleware should use to construct meters
    pub fn with_meter_provider(mut self, meter_provider: impl MeterProvider) -> Self {
        self.meter = Some(get_versioned_meter(meter_provider));
        self
    }
    
    pub fn use_paths_in_route_label( mut self, use_paths_in_route_label: bool ) -> Self {
        self.use_paths_in_route_label = use_paths_in_route_label;
        self
    }

    /// Build the `RequestMetrics` middleware
    pub fn build(self) -> RequestMetrics {
        let meter = self
            .meter
            .unwrap_or_else(|| get_versioned_meter(global::meter_provider()));

        RequestMetrics {
            route_formatter: self.route_formatter,
            metrics: Arc::new(Metrics::new(meter)),
            use_paths_in_route_label: self.use_paths_in_route_label,
        }
    }
}

/// construct meters for this crate
fn get_versioned_meter(meter_provider: impl MeterProvider) -> Meter {
    meter_provider.versioned_meter(
        "actix_web_opentelemetry",
        Some(env!("CARGO_PKG_VERSION")),
        Some(opentelemetry_semantic_conventions::SCHEMA_URL),
        None,
    )
}

/// Request metrics tracking
///
/// # Examples
///
/// ```no_run
/// use actix_web::{dev, http, web, App, HttpRequest, HttpServer};
/// use actix_web_opentelemetry::{PrometheusMetricsHandler, RequestMetrics, RequestTracing};
/// use opentelemetry_api::global;
/// use opentelemetry_sdk::metrics::MeterProvider;
///
/// #[actix_web::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     // Configure prometheus or your preferred metrics service
///     let registry = prometheus::Registry::new();
///     let exporter = opentelemetry_prometheus::exporter()
///         .with_registry(registry.clone())
///         .build()?;
///
///     // set up your meter provider with your exporter(s)
///     let provider = MeterProvider::builder()
///         .with_reader(exporter)
///         .build();
///     global::set_meter_provider(provider);
///
///     // Run actix server, metrics are now available at http://localhost:8080/metrics
///     HttpServer::new(move || {
///         App::new()
///             .wrap(RequestTracing::new())
///             .wrap(RequestMetrics::default())
///             .route("/metrics", web::get().to(PrometheusMetricsHandler::new(registry.clone())))
///         })
///         .bind("localhost:8080")?
///         .run()
///         .await?;
///
///     Ok(())
/// }
/// ```
#[derive(Clone, Debug)]
pub struct RequestMetrics {
    route_formatter: Option<Arc<dyn RouteFormatter + Send + Sync + 'static>>,
    metrics: Arc<Metrics>,
    use_paths_in_route_label: bool,
}

impl RequestMetrics {
    /// Create a builder to configure this middleware
    pub fn builder() -> RequestMetricsBuilder {
        RequestMetricsBuilder::new()
    }
}

impl Default for RequestMetrics {
    fn default() -> Self {
        RequestMetrics::builder().build()
    }
}

impl<S, B> dev::Transform<S, dev::ServiceRequest> for RequestMetrics
where
    S: dev::Service<
        dev::ServiceRequest,
        Response = dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    S::Future: 'static,
    B: 'static,
{
    type Response = dev::ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = RequestMetricsMiddleware<S>;
    type InitError = ();
    type Future = future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        let service = RequestMetricsMiddleware {
            service,
            metrics: self.metrics.clone(),
            route_formatter: self.route_formatter.clone(),
            use_paths_in_route_label: self.use_paths_in_route_label,
        };

        future::ok(service)
    }
}

/// Request metrics middleware
#[allow(missing_debug_implementations)]
pub struct RequestMetricsMiddleware<S> {
    service: S,
    metrics: Arc<Metrics>,
    route_formatter: Option<Arc<dyn RouteFormatter + Send + Sync + 'static>>,
    use_paths_in_route_label: bool,
}

impl<S, B> dev::Service<dev::ServiceRequest> for RequestMetricsMiddleware<S>
where
    S: dev::Service<
        dev::ServiceRequest,
        Response = dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    S::Future: 'static,
    B: 'static,
{
    type Response = dev::ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    dev::forward_ready!(service);

    fn call(&self, req: dev::ServiceRequest) -> Self::Future {
        let timer = SystemTime::now();

        
            
        
        let mut http_target =
            if self.use_paths_in_route_label{
                Some( req.path().to_string() )
            }else{
                req.match_pattern()
            }
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed("default"));
            

        if let Some(formatter) = &self.route_formatter {
            http_target = Cow::Owned(formatter.format(&http_target));
        }

        let mut attributes = metrics_attributes_from_request(&req, http_target);
        self.metrics.http_server_active_requests.add(1, &attributes);

        let content_length = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|len| len.to_str().ok().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        self.metrics
            .http_server_request_size
            .record(content_length, &attributes);

        let request_metrics = self.metrics.clone();
        Box::pin(self.service.call(req).map(move |res| {
            request_metrics
                .http_server_active_requests
                .add(-1, &attributes);

            // Ignore actix errors for metrics
            if let Ok(res) = res {
                attributes.push(HTTP_RESPONSE_STATUS_CODE.i64(res.status().as_u16() as i64));
                let response_size = res
                    .response()
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|len| len.to_str().ok().and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                request_metrics
                    .http_server_response_size
                    .record(response_size, &attributes);

                request_metrics.http_server_duration.record(
                    timer.elapsed().map(|t| t.as_secs_f64()).unwrap_or_default(),
                    &attributes,
                );

                Ok(res)
            } else {
                res
            }
        }))
    }
}

#[cfg(feature = "metrics-prometheus")]
#[cfg_attr(docsrs, doc(cfg(feature = "metrics-prometheus")))]
pub(crate) mod prometheus {
    use actix_web::{dev, http::StatusCode};
    use futures_util::future::{self, LocalBoxFuture};
    use opentelemetry_api::{global, metrics::MetricsError};
    use prometheus::{Encoder, Registry, TextEncoder};

    /// Prometheus request metrics service
    #[derive(Clone, Debug)]
    pub struct PrometheusMetricsHandler {
        prometheus_registry: Registry,
    }

    impl PrometheusMetricsHandler {
        /// Build a route to serve Prometheus metrics
        pub fn new(registry: Registry) -> Self {
            Self {
                prometheus_registry: registry,
            }
        }
    }

    impl PrometheusMetricsHandler {
        fn metrics(&self) -> String {
            let encoder = TextEncoder::new();
            let metric_families = self.prometheus_registry.gather();
            let mut buf = Vec::new();
            if let Err(err) = encoder.encode(&metric_families[..], &mut buf) {
                global::handle_error(MetricsError::Other(err.to_string()));
            }

            String::from_utf8(buf).unwrap_or_default()
        }
    }

    impl dev::Handler<actix_web::HttpRequest> for PrometheusMetricsHandler {
        type Output = Result<actix_web::HttpResponse<String>, actix_web::error::Error>;
        type Future = LocalBoxFuture<'static, Self::Output>;

        fn call(&self, _req: actix_web::HttpRequest) -> Self::Future {
            Box::pin(future::ok(actix_web::HttpResponse::with_body(
                StatusCode::OK,
                self.metrics(),
            )))
        }
    }
}
