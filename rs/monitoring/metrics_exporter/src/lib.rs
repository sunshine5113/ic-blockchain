use hyper::{server::conn::Http, service::service_fn, Body, Response};
use ic_config::metrics::{Config, Exporter};
use ic_crypto_tls_interfaces::TlsHandshake;
use ic_interfaces::registry::RegistryClient;
use ic_metrics::registry::MetricsRegistry;
use prometheus::{Encoder, TextEncoder};
use slog::{error, trace, warn};
use std::net::SocketAddr;
use std::string::String;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

const LOG_INTERVAL_SECS: u64 = 30;

/// The type of a metrics runtime implementation.
pub struct MetricsRuntimeImpl {
    rt_handle: tokio::runtime::Handle,
    config: Config,
    metrics_registry: MetricsRegistry,
    crypto_tls: Option<(Arc<dyn RegistryClient>, Arc<dyn TlsHandshake + Send + Sync>)>,
    log: slog::Logger,
}

/// An implementation of the metrics runtime type.
impl MetricsRuntimeImpl {
    pub fn new(
        rt_handle: tokio::runtime::Handle,
        config: Config,
        metrics_registry: MetricsRegistry,
        registry_client: Arc<dyn RegistryClient>,
        crypto: Arc<dyn TlsHandshake + Send + Sync>,
        log: &slog::Logger,
    ) -> Self {
        let log = log.new(slog::o!("Application" => "MetricsRuntime"));

        let metrics = Self {
            rt_handle,
            config,
            metrics_registry,
            crypto_tls: Some((registry_client, crypto)),
            log,
        };

        match metrics.config.exporter {
            Exporter::Http(socket_addr) => metrics.start_http(socket_addr),
            Exporter::Log => metrics.start_log(),
            Exporter::File(_) => {}
        };

        metrics
    }

    /// Create a MetricsRuntimeImpl supporting only HTTP for insecure use cases
    /// e.g. testing binaries where the node certificate may not be available.
    pub fn new_insecure(
        rt_handle: tokio::runtime::Handle,
        config: Config,
        metrics_registry: MetricsRegistry,
        log: &slog::Logger,
    ) -> Self {
        let log = log.new(slog::o!("Application" => "MetricsRuntime"));

        let metrics = Self {
            rt_handle,
            config,
            metrics_registry,
            crypto_tls: None,
            log,
        };

        match metrics.config.exporter {
            Exporter::Http(socket_addr) => metrics.start_http(socket_addr),
            Exporter::Log => metrics.start_log(),
            Exporter::File(_) => {}
        };

        metrics
    }

    /// Spawn a background task which dump the metrics to the log.  This task
    /// does not terminate and if/when we support clean shutdown this task will
    /// need to be joined.
    fn start_log(&self) {
        let log = self.log.clone();
        let metrics_registry = self.metrics_registry.clone();
        self.rt_handle.spawn(async move {
            let encoder = TextEncoder::new();
            let mut interval = tokio::time::interval(Duration::from_secs(LOG_INTERVAL_SECS));
            loop {
                interval.tick().await;

                let mut buffer = vec![];
                let metric_families = metrics_registry.prometheus_registry().gather();
                encoder.encode(&metric_families, &mut buffer).unwrap();
                let metrics = String::from_utf8(buffer).unwrap();
                trace!(log, "{}", metrics);
            }
        });
    }

    /// Spawn a background task to accept and handle metrics connections.  This
    /// task does not terminate and if/when we support clean shutdown this
    /// task will need to be joined.
    fn start_http(&self, address: SocketAddr) {
        let metrics_registry = self.metrics_registry.clone();
        let log = self.log.clone();

        let aservice = service_fn(move |_req| {
            // Clone again to ensure that `metrics_registry` outlives this closure.
            let metrics_registry = metrics_registry.clone();
            let encoder = TextEncoder::new();

            async move {
                let metric_families = metrics_registry.prometheus_registry().gather();
                let mut buffer = vec![];
                encoder.encode(&metric_families, &mut buffer).unwrap();
                Ok::<_, hyper::Error>(Response::new(Body::from(buffer)))
            }
        });

        let crypto_tls = self.crypto_tls.clone();
        // Temporarily listen on [::] so that we accept both IPv4 and IPv6 connections.
        // This requires net.ipv6.bindv6only = 0.  TODO: revert this once we have rolled
        // out IPv6 in prometheus and ic_p8s_service_discovery.
        let mut addr = "[::]:9090".parse::<SocketAddr>().unwrap();
        addr.set_port(address.port());
        self.rt_handle.spawn(async move {
            let listener = match TcpListener::bind(&addr).await {
                Err(e) => {
                    error!(log, "HTTP exporter server error: {}", e);
                    return;
                }
                Ok(listener) => listener,
            };
            let http = Http::new();
            loop {
                let log = log.clone();
                let http = http.clone();
                let aservice = aservice.clone();
                let crypto_tls = crypto_tls.clone();
                if let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut b = [0_u8; 1];
                        if stream.peek(&mut b).await.is_ok() && b[0] == 22 {
                            if let Some((registry_client, crypto)) = crypto_tls {
                                // Note: the unwrap() can't fail since we tested Some(crypto)
                                // above.
                                let registry_version = registry_client.get_latest_version();
                                match crypto
                                    .perform_tls_server_handshake_without_client_auth(
                                        stream,
                                        registry_version,
                                    )
                                    .await
                                {
                                    Err(e) => warn!(log, "TLS error: {}", e),
                                    Ok(stream) => {
                                        if let Err(e) =
                                            http.serve_connection(stream, aservice).await
                                        {
                                            trace!(log, "Connection error: {}", e);
                                        }
                                    }
                                };
                            }
                        } else {
                            // Fallback to Http.
                            if let Err(e) = http.serve_connection(stream, aservice).await {
                                trace!(log, "Connection error: {}", e);
                            }
                        }
                    });
                }
            }
        });
    }
}

impl Drop for MetricsRuntimeImpl {
    fn drop(&mut self) {
        if let Exporter::File(ref path) = self.config.exporter {
            match std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(path)
            {
                Ok(mut file) => {
                    let encoder = TextEncoder::new();
                    let metric_families = self.metrics_registry.prometheus_registry().gather();
                    encoder
                        .encode(&metric_families, &mut file)
                        .unwrap_or_else(|err| {
                            error!(
                                self.log,
                                "Failed to encode metrics to file {}: {}",
                                path.display(),
                                err
                            );
                        });
                }
                Err(err) => {
                    error!(self.log, "Failed to open file {}: {}", path.display(), err);
                }
            }
        }
    }
}
