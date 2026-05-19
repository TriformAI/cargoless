//! #246 Wave-1 5a — Telemetry foundation + global SDK init.
//!
//! Binary-side OTEL setup consuming [`cargoless_core::TelemetryConfig`] (5b,
//! pure data). Stands up the `tracing` ↔ `opentelemetry_sdk` ↔ OTLP-exporter
//! stack per the canonical SigNoz Rust OTel guide:
//!
//! * Transport: **OTLP HTTP/protobuf via async `reqwest-client`** (not
//!   `grpc-tonic`). Wire-shape equivalent at the SigNoz collector; matches
//!   the documented vendor path.
//! * Runtime: **tokio current-thread**, owned by `serve.rs` via
//!   `runtime.block_on(async { ... })`. The daemon's existing sync code
//!   runs unchanged inside the async context — tokio is the substrate the
//!   OTel SDK needs, nothing more. Subcommands other than `serve` remain
//!   pure-sync (they never init telemetry, so they never need a runtime).
//!
//! Cores stay log-free (no `tracing` macros in `cargoless-core`). All
//! instrumentation lands at the binary call sites in `servedrv.rs` (5c).
//!
//! ## Fail-soft contract (load-bearing — plan §5a)
//!
//! Telemetry **must never wedge the daemon**. The init path can fail in
//! many places — exporter endpoint unreachable, malformed config, SDK
//! init panic. Every failure path returns an **inert [`ShutdownHandle`]**
//! that drops cleanly: the daemon continues with stderr-only logging
//! (matches the dep-free `[cargoless:obs]` eprintlns from #247 — they
//! stay in place as the always-on fallback).
//!
//! ## Default path: no-op
//!
//! If [`TelemetryConfig::enabled()`] returns `false` (no endpoint
//! configured), `init_telemetry` returns immediately with an inert handle.
//! Zero OTEL overhead for local `cargoless check` / ad-hoc `serve`.
//!
//! ## Wave-1 scope
//!
//! * DOES: global `SdkTracerProvider` init, `tracing-subscriber` stack
//!   with the OTel layer (5e log↔trace correlation surface), W3C
//!   `TraceContextPropagator` registration, ordered shutdown via
//!   provider's `shutdown()` (5f), `record_exception` helper (sets
//!   `otel.status_code=ERROR` + structured error/exception attrs).
//! * DOES NOT: individual span instrumentation (that's 5c, wired at
//!   servedrv call sites). The 5 KEYSTONE spans (`ra.spawn`,
//!   `ra.respawn`, `overlay.reset` event, `overlay.switch`,
//!   `verdict.publish`) live in `servedrv.rs` — they emit via the
//!   `tracing` macros, which this module's global subscriber bridges to
//!   OTEL.
//! * DOES NOT: metrics (5d / Wave 2 — Wave-1 is traces + logs only).
//!
//! ## Caller contract for init
//!
//! `init_telemetry` MUST be invoked from inside a tokio runtime context
//! (typically via `runtime.block_on(async { ... })` in `serve.rs`). The
//! OTel SDK's batch exporter needs a runtime handle to spawn its export
//! task. If called outside a runtime AND `cfg.enabled()` is true, init
//! takes the fail-soft path: stderr warning + inert handle, no panic.

use cargoless_core::TelemetryConfig;

// ────────────────────────────────────────────────────────────────────────
// Public surface — what `main.rs` / `servedrv.rs` consume.
// ────────────────────────────────────────────────────────────────────────

/// Opaque handle returned by [`init_telemetry`]. Holds the
/// `SdkTracerProvider` (when one was installed) so [`shutdown_telemetry`]
/// can drive the flush + shutdown. Drop is best-effort — calling
/// [`shutdown_telemetry`] explicitly is the load-bearing path.
pub struct ShutdownHandle {
    /// `None` ⇒ inert handle (no init ran; nothing to flush).
    inner: Option<HandleInner>,
}

struct HandleInner {
    #[cfg(feature = "telemetry")]
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    service_name: String,
}

impl ShutdownHandle {
    fn inert() -> Self {
        Self { inner: None }
    }

    /// `true` if init actually installed a provider (vs no-endpoint
    /// no-op). Useful for diagnostics; not load-bearing in the
    /// shutdown path so allowed-dead until a diagnostics consumer
    /// wires it up.
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }
}

impl Drop for ShutdownHandle {
    fn drop(&mut self) {
        // Best-effort flush on drop — only fires if the caller didn't
        // already call `shutdown_telemetry`. The explicit path is
        // preferred because it carries error reporting.
        if let Some(_inner) = self.inner.take() {
            #[cfg(feature = "telemetry")]
            {
                // SDK 0.31+: SdkTracerProvider::shutdown() returns
                // Result<(), OTelSdkError>. Ignore on drop (no caller to
                // route the error to).
                let _ = _inner.tracer_provider.shutdown();
            }
        }
    }
}

/// Initialize the OTEL + tracing stack from `cfg`. **Never panics; never
/// returns an error type** — fail-soft contract: every failure path
/// produces an inert handle + stderr warning, daemon continues.
///
/// Behaviour:
///
/// * `cfg.enabled() == false` (no endpoint) → return inert handle
///   immediately (zero overhead, no log lines).
/// * `cfg.enabled() == true` → attempt OTLP exporter init + global
///   `TracerProvider` install + `tracing-subscriber` registration.
///   Success → active handle. Failure at any step → stderr warning +
///   inert handle.
///
/// **Call exactly once at process startup, inside a tokio runtime
/// context** (`runtime.block_on(async { let h = init_telemetry(&cfg); ... })`).
/// `tracing::subscriber::set_global_default` is one-shot; a second call
/// is harmless (no-op + warning).
#[allow(unused_variables)]
pub fn init_telemetry(cfg: &TelemetryConfig) -> ShutdownHandle {
    // Default-disabled: no endpoint ⇒ no SDK init. Zero overhead for
    // local invocations. The single load-bearing predicate from 5b.
    if !cfg.enabled() {
        return ShutdownHandle::inert();
    }

    #[cfg(feature = "telemetry")]
    {
        match try_init(cfg) {
            Ok(handle) => handle,
            Err(why) => {
                // Fail-soft: never wedge the daemon. The stderr-only
                // `[cargoless:obs]` eprintlns from #247 remain the
                // ops-without-collector fallback signal.
                eprintln!(
                    "[cargoless:telemetry] init failed ({why}); continuing \
                     without OTEL export. Stderr observability lines remain."
                );
                ShutdownHandle::inert()
            }
        }
    }
    #[cfg(not(feature = "telemetry"))]
    {
        eprintln!(
            "[cargoless:telemetry] endpoint configured but binary was \
             built without the `telemetry` feature; ignoring. Rebuild \
             with `--features telemetry` to enable OTEL export."
        );
        ShutdownHandle::inert()
    }
}

/// Ordered flush + shutdown. Hook into the existing #198 SIGTERM funnel
/// in servedrv.rs's main loop: telemetry shutdown BEFORE orphan reap so
/// pending spans egress before daemon termination.
///
/// Idempotent: calling on an inert handle (or twice) is a no-op.
pub fn shutdown_telemetry(mut handle: ShutdownHandle) {
    let Some(inner) = handle.inner.take() else {
        return; // inert
    };

    #[cfg(feature = "telemetry")]
    {
        // SDK 0.31+: shutdown() returns Result<(), OTelSdkError>.
        // Drives synchronous flush of all batch processors + closes the
        // exporter. Errors here are diagnostic-only; the daemon
        // continues to termination either way.
        if let Err(e) = inner.tracer_provider.shutdown() {
            eprintln!(
                "[cargoless:telemetry] shutdown error for service={}: {e:?}",
                inner.service_name
            );
        }
    }

    #[cfg(not(feature = "telemetry"))]
    {
        let _ = inner;
    }
}

/// Record a Rust error onto a tracing span, setting the load-bearing
/// `otel.status_code=ERROR` + structured `error.*` / `exception.*` attrs.
/// Port of physics telemetry.rs:836-846.
///
/// **Why load-bearing:** SigNoz reads `otel.status_code` for the
/// `hasError=true` filter — without it, failed spans are
/// indistinguishable from succeeded ones (real incident pattern in
/// physics file).
#[allow(dead_code, unused_variables)] // Wave-2 — first error-attaching span site lands then.
pub fn record_exception(span: &tracing::Span, err: &dyn std::error::Error) {
    let kind = std::any::type_name_of_val(err);
    let msg = err.to_string();
    span.record("otel.status_code", "ERROR");
    span.record("error.kind", kind);
    span.record("error.message", msg.as_str());
    span.record("exception.type", kind);
    span.record("exception.message", msg.as_str());
}

// ────────────────────────────────────────────────────────────────────────
// Internal — feature-gated SDK setup.
// ────────────────────────────────────────────────────────────────────────

#[cfg(feature = "telemetry")]
fn try_init(cfg: &TelemetryConfig) -> Result<ShutdownHandle, String> {
    use opentelemetry::KeyValue;
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let endpoint = cfg
        .otel_endpoint
        .as_deref()
        .ok_or_else(|| "enabled() lied: endpoint absent".to_string())?;

    // OTLP HTTP/protobuf exporter — async reqwest-client transport per
    // SigNoz canonical Rust guide. The exporter spawns its export task
    // on the ambient tokio runtime (provided by serve.rs's block_on).
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_protocol(Protocol::HttpBinary)
        .build()
        .map_err(|e| format!("OTLP exporter build failed: {e}"))?;

    // Resource attrs per plan §5a: service.name (env-overridable),
    // service.version (compile-time), `cargoless.build_id` (task #89
    // consolidated build identifier).
    let resource = Resource::builder()
        .with_attributes(vec![
            KeyValue::new("service.name", cfg.otel_service_name.clone()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("cargoless.build_id", cargoless_core::BUILD_ID),
        ])
        .build();

    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::TraceIdRatioBased(cfg.otel_sampler_arg))
        .build();

    // Register global propagator BEFORE installing the provider so any
    // span emitted between propagator-set and provider-set still gets
    // proper context propagation.
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Install the provider globally so #[instrument] / tracing macros
    // get the active tracer.
    global::set_tracer_provider(tracer_provider.clone());

    let tracer = tracer_provider.tracer("cargoless");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // tracing-subscriber stack: env-filter + OTEL layer + fmt layer for
    // local stderr. fmt is intentionally noisy so an operator running
    // `serve` foreground still sees the verdict stream alongside the
    // OTEL export (matches the existing `[cargoless:obs]` pattern).
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("info,cargoless={}", cfg.otel_log_level)));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(fmt::layer().with_writer(std::io::stderr));

    // set_global_default is one-shot. Map a "second-init" failure to
    // soft-fail (e.g. a serve→watch handoff in tests).
    subscriber
        .try_init()
        .map_err(|e| format!("tracing-subscriber init failed: {e}"))?;

    Ok(ShutdownHandle {
        inner: Some(HandleInner {
            tracer_provider,
            service_name: cfg.otel_service_name.clone(),
        }),
    })
}

// ────────────────────────────────────────────────────────────────────────
// Tests — the no-op / inert path is the most-common case; cover it
// exhaustively. The active path is integration-tested downstream
// (against a real SigNoz collector during Stage-1 dogfood per plan AC4).
// ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_returns_inert_when_no_endpoint() {
        let cfg = TelemetryConfig::defaults();
        assert!(!cfg.enabled(), "precondition: defaults are disabled");
        let h = init_telemetry(&cfg);
        assert!(!h.is_active(), "no endpoint ⇒ inert handle");
    }

    #[test]
    fn shutdown_on_inert_handle_is_noop() {
        let h = init_telemetry(&TelemetryConfig::defaults());
        shutdown_telemetry(h);
    }

    #[test]
    fn double_shutdown_on_inert_is_noop() {
        let h1 = init_telemetry(&TelemetryConfig::defaults());
        shutdown_telemetry(h1);
        let h2 = init_telemetry(&TelemetryConfig::defaults());
        shutdown_telemetry(h2);
    }

    #[test]
    fn drop_inert_handle_is_clean() {
        let h = init_telemetry(&TelemetryConfig::defaults());
        drop(h);
    }

    #[test]
    fn record_exception_on_disabled_span_is_safe() {
        // Without an active subscriber, tracing::Span::current() is
        // disabled — record_exception MUST still be safe to call
        // (fail-soft extends to per-call sites, not just init).
        let span = tracing::Span::current();
        let err: Box<dyn std::error::Error> =
            std::io::Error::new(std::io::ErrorKind::Other, "test").into();
        record_exception(&span, err.as_ref());
    }
}
