//! #246 Wave-1 5a — Telemetry foundation + global SDK init.
//!
//! Binary-side OTEL setup consuming [`cargoless_core::TelemetryConfig`] (5b,
//! pure data). Stands up the `tracing` ↔ `opentelemetry_sdk` ↔ OTLP-exporter
//! stack per the canonical SigNoz Rust OTel guide:
//!
//! * Transport: **OTLP HTTP/protobuf via blocking `reqwest-blocking-client`**
//!   (not `grpc-tonic`, not async `reqwest-client`). Wire-shape equivalent
//!   at the SigNoz collector; matches the documented vendor path. The
//!   **blocking** client is load-bearing — see INFRA-49 follow-up below.
//!
//! ## INFRA-49 — why blocking transport
//!
//! No tokio reactor may be required on the threads that end spans (the
//! serve-loop / verdict threads are plain sync threads). With the async
//! `reqwest-client` feature the export future needs an ambient runtime —
//! the "no reactor running" panic class, observed live on cargoless 0.2.7
//! in both the batch-processor-thread and inline-export shapes. The
//! blocking client (`reqwest::blocking::Client`) has no runtime
//! requirement at call time.
//!
//! ## A5 — queued background export (collector outage ≠ verdict stall)
//!
//! Blocking transport alone still exported INLINE on whatever thread
//! ended the span (`with_simple_exporter`): a SigNoz collector outage
//! stalled the span-ending thread — including the serve-loop thread that
//! publishes verdicts — for up to the 10s HTTP timeout per span. The
//! default is now the [`queue`] module's `QueuedSpanProcessor`: `on_end`
//! is a bounded `try_send` (overflow drops, power-of-two-sampled stderr
//! accounting); one dedicated `cargoless-otlp` worker thread owns the
//! exporter and is the only thread that ever blocks on the collector
//! (INFRA-49 holds — it drives the export future with
//! `futures_executor::block_on`, the exact execution shape
//! `SimpleSpanProcessor` uses inline). Shutdown drains for at most 2s,
//! then detaches. `CARGOLESS_OTLP_INLINE=1` restores the previous inline
//! construction (one-release rollback path).
//! * Runtime: **tokio**, owned by `serve.rs`. Span export no longer needs
//!   it (see A5 above); it remains the substrate for
//!   [`shutdown_telemetry`]'s explicit 5s flush-timeout machinery.
//!   Subcommands other than `serve` remain pure-sync (they never init
//!   telemetry, so they never need a runtime).
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
//! (serve.rs owns the runtime + `runtime.enter()` guard). Span export
//! does not need the runtime (the `cargoless-otlp` worker is a plain std
//! thread); the requirement comes from [`shutdown_telemetry`]'s explicit
//! 5s flush-timeout machinery (`Handle::current()` +
//! `tokio::time::timeout`).

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
        // INFRA-54 diagnostic: surface the inert path explicitly so the
        // operator can distinguish "I never tried to init" from "init
        // succeeded silently but spans don't reach the collector." The
        // *zero* eprintln on success that the original 5a design used
        // turned out to be the diagnostic gap that hid INFRA-54 root
        // cause from observation. Always-on now (eprintln is cheap and
        // happens once at startup, not per-span).
        eprintln!(
            "[cargoless:telemetry] inert (no endpoint configured — set \
             OTEL_EXPORTER_OTLP_ENDPOINT to enable). service_name={}",
            cfg.otel_service_name
        );
        return ShutdownHandle::inert();
    }

    #[cfg(feature = "telemetry")]
    {
        match try_init(cfg) {
            Ok(handle) => {
                // INFRA-54 diagnostic: the original 5a design left the
                // success path silent (no eprintln, no info line). When
                // spans turned up missing on the production rollout we
                // had no way to tell whether init had run at all vs.
                // run-and-export-silently-failed. Explicit success log
                // makes the boundary visible. Includes endpoint +
                // service name so dashboards-side mismatches are
                // immediately diagnosable.
                eprintln!(
                    "[cargoless:telemetry] init OK \
                     endpoint={} service={} sampler_arg={}",
                    cfg.otel_endpoint.as_deref().unwrap_or("(unset)"),
                    cfg.otel_service_name,
                    cfg.otel_sampler_arg
                );
                handle
            }
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

/// Ordered flush + shutdown with explicit **5s timeout**. Hook into the
/// existing #198 SIGTERM funnel in servedrv.rs's main loop: telemetry
/// shutdown BEFORE orphan reap so pending spans egress before daemon
/// termination.
///
/// The 5s timeout is plan §5f's load-bearing flush budget — a slow /
/// wedged collector MUST NOT block the daemon from terminating. The
/// explicit `tokio::time::timeout` makes the budget visible-in-source
/// rather than implicit in SDK-default behavior (which varies across
/// `opentelemetry_sdk` versions). If the timeout elapses, a one-line
/// stderr warning surfaces the observability-discipline event so a
/// degraded collector path is loud not silent.
///
/// Idempotent: calling on an inert handle (or twice) is a no-op.
///
/// **Caller contract:** invoked from inside the same tokio runtime
/// context as `init_telemetry` (typically via `serve.rs`'s
/// `runtime.enter()` guard). Calling outside a runtime when telemetry
/// is active panics in `tokio::time::timeout` — but that path is
/// unreachable because the runtime guard brackets both init+shutdown
/// by construction in serve.rs.
pub fn shutdown_telemetry(mut handle: ShutdownHandle) {
    let Some(inner) = handle.inner.take() else {
        return; // inert
    };

    #[cfg(feature = "telemetry")]
    {
        use std::time::Duration;
        // Wrap the SDK shutdown in a 5s budget. The shutdown body is
        // synchronous from the SDK's perspective; we run it inside
        // `spawn_blocking` so `timeout` has an async future to gate.
        // The whole shutdown call is best-effort — diagnostic on
        // either failure mode (timeout elapsed OR SDK error).
        let provider = inner.tracer_provider;
        let service = inner.service_name.clone();
        let handle = tokio::runtime::Handle::current();
        let outcome = handle.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::task::spawn_blocking(move || provider.shutdown()),
            )
            .await
        });
        match outcome {
            Ok(Ok(Ok(()))) => { /* clean flush */ }
            Ok(Ok(Err(e))) => {
                eprintln!("[cargoless:telemetry] shutdown error for service={service}: {e:?}");
            }
            Ok(Err(join_err)) => {
                eprintln!(
                    "[cargoless:telemetry] shutdown task panic for service={service}: {join_err}"
                );
            }
            Err(_elapsed) => {
                eprintln!(
                    "[cargoless:telemetry] shutdown TIMEOUT (>5s) for service={service}; \
                     pending spans may be lost — investigate collector reachability."
                );
            }
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

    // OTLP HTTP/protobuf exporter — blocking reqwest transport
    // (`reqwest-blocking-client` feature): no reactor needed at export
    // time, so the `cargoless-otlp` worker drives it as a plain thread.
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_protocol(Protocol::HttpBinary)
        // 10 s cap matches physics telemetry.rs:156. Without this the blocking
        // reqwest call has no deadline — a wedged in-cluster collector would
        // stall the `cargoless-otlp` export worker indefinitely (and, under
        // CARGOLESS_OTLP_INLINE=1, the span-ending thread itself).
        .with_timeout(std::time::Duration::from_secs(10))
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

    // A5: default = QueuedSpanProcessor (bounded queue + dedicated
    // `cargoless-otlp` worker thread — see the `queue` module header for
    // the full INFRA-49 / never-stall-the-verdict-path rationale).
    //
    // Why NOT the SDK's own `with_batch_exporter`: its BatchSpanProcessor
    // thread needs an ambient Tokio runtime to drive async HTTP export.
    // Observed live on cargoless 0.2.7 (PR #22 + tf-multiverse PR #3526):
    //
    //     thread 'OpenTelemetry.Traces.BatchProcessor' panicked at ...:
    //     there is no reactor running, must be called from the context
    //     of a Tokio 1.x runtime
    //
    // after which the SDK entered permanent shutdown and silently dropped
    // every subsequent span. (With the blocking reqwest transport now in
    // use that specific panic is gone, but the queue processor keeps the
    // export off the span-ending thread AND bounds the shutdown drain at
    // 2s instead of the SDK default 5s-per-processor.)
    //
    // Why NOT `with_simple_exporter` (the previous default): it exports
    // INLINE on whatever thread ends the span — a SigNoz collector outage
    // stalls the serve-loop verdict path for up to the 10s HTTP timeout
    // per span. That deployment-stall class is what A5 removes.
    //
    // CARGOLESS_OTLP_INLINE=1 keeps the old inline construction as a
    // one-release rollback path.
    let inline = std::env::var("CARGOLESS_OTLP_INLINE").is_ok_and(|v| v == "1");
    let builder = SdkTracerProvider::builder().with_resource(resource);
    let builder = if inline {
        eprintln!(
            "[cargoless:telemetry] CARGOLESS_OTLP_INLINE=1 — inline (simple) \
             span export; span ends block on the collector."
        );
        builder.with_simple_exporter(exporter)
    } else {
        let processor = queue::QueuedSpanProcessor::spawn(exporter)
            .map_err(|e| format!("OTLP export worker spawn failed: {e}"))?;
        builder.with_span_processor(processor)
    };
    let tracer_provider = builder
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
// A5 — queued background OTLP span export.
// ────────────────────────────────────────────────────────────────────────

#[cfg(feature = "telemetry")]
mod queue {
    //! [`QueuedSpanProcessor`] — bounded-queue + dedicated-worker span
    //! processor, the default instead of `with_simple_exporter` (A5).
    //!
    //! Contract, ranked:
    //!
    //! 1. **`on_end` never blocks** the span-ending thread. The serve
    //!    loop's single thread ends the verdict spans; a SigNoz collector
    //!    outage must cost it nothing. `on_end` is a `try_send` into a
    //!    bounded channel — overflow DROPS the span (spans are
    //!    diagnostics; they must never block or fail the verdict path),
    //!    with power-of-two-sampled stderr accounting (1st, 2nd, 4th,
    //!    8th... drop) so a sustained outage is loud but not spammy.
    //! 2. **Only the worker thread blocks on the collector.** One
    //!    dedicated `cargoless-otlp` std thread owns the exporter and
    //!    drives each batch with `futures_executor::block_on` — the
    //!    exact execution shape `SimpleSpanProcessor::on_end` uses
    //!    inline, so INFRA-49 (no tokio reactor required on the threads
    //!    that end spans) holds by construction: the worker is a plain
    //!    std thread and the blocking-reqwest export future completes
    //!    without a reactor.
    //! 3. **Shutdown/flush are bounded.** The drain gets at most
    //!    [`DRAIN_BUDGET`] (2s); past the deadline the worker is
    //!    detached and the call returns. A wedged collector costs CLI
    //!    shutdown ≤2s, not 10s-per-pending-span.
    //!
    //! Control messages (resource, flush, shutdown) share the one FIFO
    //! channel with spans, which buys ordering for free: the resource
    //! set by `TracerProviderBuilder::build()` is applied before any
    //! span export, and a shutdown sentinel drains exactly the spans
    //! enqueued before it.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
    use std::thread;
    use std::time::{Duration, Instant};

    use opentelemetry::Context;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
    use opentelemetry_sdk::trace::{Span, SpanData, SpanExporter, SpanProcessor};

    /// Queue bound. Matches the OTel BatchSpanProcessor default
    /// (`OTEL_BSP_MAX_QUEUE_SIZE` = 2048); at cargoless's verdict-cadence
    /// span volume that is minutes of headroom across an outage.
    /// `pub(super)` so the overflow test can size its push loop off the
    /// real bound instead of a copy that could drift.
    pub(super) const QUEUE_CAP: usize = 2048;
    /// Max spans per export call.
    const BATCH_MAX: usize = 64;
    /// Max time a span waits in an unfilled batch before export.
    const BATCH_WINDOW: Duration = Duration::from_millis(500);
    /// Shutdown/flush drain budget — past it, detach the worker and
    /// return (kills the multi-second CLI shutdown tax).
    const DRAIN_BUDGET: Duration = Duration::from_secs(2);

    /// One channel carries spans + control so ordering is FIFO-exact.
    /// SpanData (~hundreds of bytes) dwarfs the control variants; boxing
    /// it would put an allocation on the span-end hot path, so take the
    /// size hit — same call the SDK's own BatchMessage makes.
    #[allow(clippy::large_enum_variant)]
    enum Msg {
        Span(SpanData),
        SetResource(Resource),
        Flush(SyncSender<()>),
        Shutdown(SyncSender<()>),
    }

    #[derive(Debug)]
    pub(super) struct QueuedSpanProcessor {
        sender: SyncSender<Msg>,
        worker: Mutex<Option<thread::JoinHandle<()>>>,
        stopped: AtomicBool,
        dropped: AtomicUsize,
        disconnect_noted: AtomicBool,
    }

    impl QueuedSpanProcessor {
        /// Spawn the `cargoless-otlp` worker that owns `exporter`. Thread
        /// spawn failure surfaces as `Err` so `try_init` can take the
        /// fail-soft inert path (daemon continues, stderr-only).
        pub(super) fn spawn<E>(exporter: E) -> std::io::Result<Self>
        where
            E: SpanExporter + 'static,
        {
            let (sender, receiver) = mpsc::sync_channel(QUEUE_CAP);
            let worker = thread::Builder::new()
                .name("cargoless-otlp".to_string())
                .spawn(move || worker_loop(exporter, receiver))?;
            Ok(Self {
                sender,
                worker: Mutex::new(Some(worker)),
                stopped: AtomicBool::new(false),
                dropped: AtomicUsize::new(0),
                disconnect_noted: AtomicBool::new(false),
            })
        }

        #[cfg(test)]
        pub(super) fn dropped_so_far(&self) -> usize {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    impl SpanProcessor for QueuedSpanProcessor {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {
            // Ignored — export concerns finished spans only.
        }

        fn on_end(&self, span: SpanData) {
            // Parity with SimpleSpanProcessor: RecordOnly (unsampled)
            // spans are recorded but never exported.
            if !span.span_context.is_sampled() {
                return;
            }
            if self.stopped.load(Ordering::Relaxed) {
                return; // post-shutdown spans are dropped silently
            }
            match self.sender.try_send(Msg::Span(span)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        eprintln!("[cargoless:otel] span queue full, dropped {n} spans so far");
                    }
                }
                Err(TrySendError::Disconnected(_)) => {
                    if !self.disconnect_noted.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "[cargoless:otel] span emitted after export worker exit; dropping"
                        );
                    }
                }
            }
        }

        fn force_flush(&self) -> OTelSdkResult {
            if self.stopped.load(Ordering::SeqCst) {
                return Err(OTelSdkError::AlreadyShutdown);
            }
            let (ack_tx, ack_rx) = mpsc::sync_channel(1);
            match self.sender.try_send(Msg::Flush(ack_tx)) {
                Ok(()) => ack_rx
                    .recv_timeout(DRAIN_BUDGET)
                    .map_err(|_| OTelSdkError::Timeout(DRAIN_BUDGET)),
                Err(TrySendError::Full(_)) => Err(OTelSdkError::InternalFailure(
                    "span queue full; flush request dropped (best-effort)".into(),
                )),
                Err(TrySendError::Disconnected(_)) => Err(OTelSdkError::AlreadyShutdown),
            }
        }

        fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
            if self.stopped.swap(true, Ordering::SeqCst) {
                return Err(OTelSdkError::AlreadyShutdown);
            }
            let budget = timeout.min(DRAIN_BUDGET);
            let deadline = Instant::now() + budget;
            let (ack_tx, ack_rx) = mpsc::sync_channel(1);
            // The sentinel competes with queued spans for capacity; retry
            // within the budget while the worker drains ahead of it.
            let mut msg = Msg::Shutdown(ack_tx);
            loop {
                match self.sender.try_send(msg) {
                    Ok(()) => break,
                    Err(TrySendError::Full(m)) => {
                        if Instant::now() >= deadline {
                            eprintln!(
                                "[cargoless:otel] shutdown: span queue still full at \
                                 deadline; detaching export worker"
                            );
                            return Err(OTelSdkError::Timeout(budget));
                        }
                        msg = m;
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        return Err(OTelSdkError::AlreadyShutdown);
                    }
                }
            }
            match ack_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => {
                    // Acked ⇒ the worker is past its final export and
                    // exiting; join is immediate.
                    if let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = h.join();
                    }
                    Ok(())
                }
                Err(_) => {
                    // Deadline elapsed mid-drain: detach. The worker keeps
                    // draining and exits on its own; the process is free to
                    // return now (pending spans are diagnostics).
                    eprintln!(
                        "[cargoless:otel] shutdown: drain exceeded {}ms; detaching \
                         export worker (pending spans may be lost)",
                        budget.as_millis()
                    );
                    Err(OTelSdkError::Timeout(budget))
                }
            }
        }

        fn set_resource(&mut self, resource: &Resource) {
            // Called by TracerProviderBuilder::build() before the provider
            // is installed (queue empty), so try_send cannot realistically
            // fail; if it ever does, degrade loudly — spans would export
            // without service.name and look unattributed in SigNoz.
            if self
                .sender
                .try_send(Msg::SetResource(resource.clone()))
                .is_err()
            {
                eprintln!(
                    "[cargoless:otel] set_resource dropped; exported spans may \
                     lack service attributes"
                );
            }
        }
    }

    /// Worker body: batch up to [`BATCH_MAX`] spans or [`BATCH_WINDOW`],
    /// then export. The ONLY place that blocks on the collector.
    fn worker_loop<E: SpanExporter>(mut exporter: E, rx: Receiver<Msg>) {
        // Same guard the SDK's batch worker takes: the exporter's own HTTP
        // stack must not generate spans about exporting spans.
        let _suppress = Context::enter_telemetry_suppressed_scope();
        let mut batch: Vec<SpanData> = Vec::with_capacity(BATCH_MAX);
        let mut window_deadline: Option<Instant> = None;
        let mut export_failures: u64 = 0;
        loop {
            let wait = window_deadline.map_or(BATCH_WINDOW, |d| {
                d.saturating_duration_since(Instant::now())
            });
            match rx.recv_timeout(wait) {
                Ok(Msg::Span(span)) => {
                    if batch.is_empty() {
                        window_deadline = Some(Instant::now() + BATCH_WINDOW);
                    }
                    batch.push(span);
                    if batch.len() >= BATCH_MAX {
                        export_batch(&exporter, &mut batch, &mut export_failures);
                        window_deadline = None;
                    }
                }
                Ok(Msg::SetResource(resource)) => exporter.set_resource(&resource),
                Ok(Msg::Flush(ack)) => {
                    export_batch(&exporter, &mut batch, &mut export_failures);
                    window_deadline = None;
                    let _ = ack.send(());
                }
                Ok(Msg::Shutdown(ack)) => {
                    // Drain stragglers racing the shutdown flag, then the
                    // final export. The shutdown caller stops waiting at
                    // its deadline regardless — a slow drain only costs
                    // this (detached) thread.
                    while let Ok(msg) = rx.try_recv() {
                        if let Msg::Span(span) = msg {
                            batch.push(span);
                            if batch.len() >= BATCH_MAX {
                                export_batch(&exporter, &mut batch, &mut export_failures);
                            }
                        }
                    }
                    export_batch(&exporter, &mut batch, &mut export_failures);
                    let _ = exporter.shutdown();
                    let _ = ack.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    export_batch(&exporter, &mut batch, &mut export_failures);
                    window_deadline = None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Processor dropped without shutdown (test paths,
                    // provider Drop): flush what we hold and exit.
                    export_batch(&exporter, &mut batch, &mut export_failures);
                    let _ = exporter.shutdown();
                    return;
                }
            }
        }
    }

    /// Export `batch` (if nonempty), blocking this worker thread only.
    /// `futures_executor::block_on` is the same executor
    /// `SimpleSpanProcessor` uses inline — the blocking-reqwest export
    /// future needs no reactor. Failures are power-of-two-sampled to
    /// stderr so a long outage stays visible without spamming.
    fn export_batch<E: SpanExporter>(exporter: &E, batch: &mut Vec<SpanData>, failures: &mut u64) {
        if batch.is_empty() {
            return;
        }
        // split_off(0) hands the spans over while keeping `batch`'s
        // allocation for the next round.
        let spans = batch.split_off(0);
        if let Err(err) = futures_executor::block_on(exporter.export(spans)) {
            *failures += 1;
            if failures.is_power_of_two() {
                eprintln!("[cargoless:otel] OTLP export failed ({failures} so far): {err}");
            }
        }
    }
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

    // ───────── #246 Wave-1 5c CATCH-1 regression sentry ─────────
    //
    // Layer-3 caught: `overlay.switch` declared `file_count` +
    // `overlay_size_bytes` as `tracing::field::Empty` but the arm body
    // never called `_span.record(...)` before exit on either path
    // (early-return on missing LSP client AND normal end-of-arm). The
    // span emitted with Empty fields under every code path. The
    // fix-forward records the fields BEFORE the lsp-present guard so
    // both exit paths carry valid attrs.
    //
    // This test enforces the structural property: declaring a span field
    // as `Empty` and then calling `span.record(name, value)` surfaces
    // that value to subscriber layers via `on_record`. The arm body's
    // correctness (that it CALLS .record before exit) is verified by
    // careful review + grep against `_span.record("file_count"` —
    // belt-and-braces with this shape test documenting the API contract.
    #[test]
    fn span_with_empty_fields_surfaces_via_on_record() {
        use std::sync::{Arc, Mutex};
        use tracing::Subscriber;
        use tracing::span::{Attributes, Id, Record};
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;

        // Local layer struct (orphan-rule compliance: Layer is foreign,
        // so the impl-type must be local). Shared state is the
        // Arc<Mutex<_>> field inside, NOT wrapping the struct.
        struct CaptureLayer {
            state: Arc<Mutex<Vec<(String, String)>>>,
        }
        impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CaptureLayer {
            fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
                let buf = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let mut v = Vis { buf };
                attrs.record(&mut v);
            }
            fn on_record(&self, _id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
                let buf = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let mut v = Vis { buf };
                values.record(&mut v);
            }
        }
        struct Vis<'a> {
            buf: std::sync::MutexGuard<'a, Vec<(String, String)>>,
        }
        impl tracing::field::Visit for Vis<'_> {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.buf.push((field.name().to_string(), value.to_string()));
            }
            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.buf.push((field.name().to_string(), value.to_string()));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.buf.push((field.name().to_string(), value.to_string()));
            }
            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                self.buf.push((field.name().to_string(), value.to_string()));
            }
            fn record_debug(
                &mut self,
                _field: &tracing::field::Field,
                _value: &dyn std::fmt::Debug,
            ) {
                // intentionally unhandled — the test asserts on the
                // numeric u64 fields the CATCH-1 fix records.
            }
        }

        let state: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            state: state.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "overlay.switch",
                worktree = "/test/wt",
                file_count = tracing::field::Empty,
                overlay_size_bytes = tracing::field::Empty,
            );
            // Mirror the arm's recording pattern exactly.
            span.record("file_count", 7u64);
            span.record("overlay_size_bytes", 4096u64);
        });

        let buf = state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            buf.iter().any(|(k, v)| k == "file_count" && v == "7"),
            "file_count not surfaced (CATCH-1 regression): captured={buf:?}"
        );
        assert!(
            buf.iter()
                .any(|(k, v)| k == "overlay_size_bytes" && v == "4096"),
            "overlay_size_bytes not surfaced (CATCH-1 regression): captured={buf:?}"
        );
    }

    // ───────── A5 — QueuedSpanProcessor contract tests ─────────
    //
    // No network anywhere: the "collector" is a stub exporter. The two
    // load-bearing properties are (1) `on_end` never blocks the
    // span-ending thread even with a wedged collector + full queue, and
    // (2) shutdown DRAINS (spans actually export) within the 2s budget.
    #[cfg(feature = "telemetry")]
    mod queue_tests {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Condvar, Mutex};
        use std::time::{Duration, Instant};

        use opentelemetry::trace::{SpanContext, SpanKind, Status, TraceState};
        use opentelemetry::{InstrumentationScope, SpanId, TraceFlags, TraceId};
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{
            SpanData, SpanEvents, SpanExporter, SpanLinks, SpanProcessor,
        };

        use super::super::queue::{QUEUE_CAP, QueuedSpanProcessor};

        /// A sampled SpanData (mirrors the SDK's own test helper —
        /// unsampled spans never reach the export path).
        fn sampled_span() -> SpanData {
            SpanData {
                span_context: SpanContext::new(
                    TraceId::from(1u128),
                    SpanId::from(1u64),
                    TraceFlags::SAMPLED,
                    false,
                    TraceState::default(),
                ),
                parent_span_id: SpanId::INVALID,
                parent_span_is_remote: false,
                span_kind: SpanKind::Internal,
                name: "test-span".into(),
                start_time: std::time::SystemTime::now(),
                end_time: std::time::SystemTime::now(),
                attributes: Vec::new(),
                dropped_attributes_count: 0,
                events: SpanEvents::default(),
                links: SpanLinks::default(),
                status: Status::Unset,
                instrumentation_scope: InstrumentationScope::default(),
            }
        }

        /// Export parks until the gate opens — models a wedged collector
        /// (the exact outage class A5 removes from the verdict path).
        #[derive(Debug)]
        struct ParkingExporter {
            gate: Arc<(Mutex<bool>, Condvar)>,
        }

        impl SpanExporter for ParkingExporter {
            async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
                let (lock, cvar) = &*self.gate;
                let mut open = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*open {
                    open = cvar.wait(open).unwrap_or_else(|e| e.into_inner());
                }
                Ok(())
            }
        }

        #[test]
        fn on_end_never_blocks_when_queue_full() {
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let processor = QueuedSpanProcessor::spawn(ParkingExporter { gate: gate.clone() })
                .expect("worker spawn");

            // The worker parks inside its first export; the queue then
            // fills and every further on_end must DROP, never block. 3x
            // the queue bound guarantees overflow under any interleaving
            // (the worker can absorb at most one batch + QUEUE_CAP).
            let t0 = Instant::now();
            for _ in 0..(3 * QUEUE_CAP) {
                processor.on_end(sampled_span());
            }
            let elapsed = t0.elapsed();
            assert!(
                elapsed < Duration::from_secs(2),
                "on_end loop took {elapsed:?} — a span end blocked on the wedged collector"
            );
            assert!(
                processor.dropped_so_far() > 0,
                "queue never overflowed — the drop path was not exercised"
            );

            // Open the gate so the worker drains + shutdown joins cleanly
            // (the detach path is not what this test is about).
            {
                let (lock, cvar) = &*gate;
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                cvar.notify_all();
            }
            let _ = processor.shutdown();
        }

        /// Records every exported span + exporter-shutdown call — proves
        /// the shutdown path drains rather than merely returning.
        #[derive(Debug)]
        struct RecordingExporter {
            spans: Arc<Mutex<Vec<SpanData>>>,
            shutdowns: Arc<AtomicUsize>,
        }

        impl SpanExporter for RecordingExporter {
            async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
                self.spans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(batch);
                Ok(())
            }

            fn shutdown_with_timeout(&mut self, _timeout: Duration) -> OTelSdkResult {
                self.shutdowns.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        #[test]
        fn shutdown_drains_within_deadline() {
            let spans = Arc::new(Mutex::new(Vec::new()));
            let shutdowns = Arc::new(AtomicUsize::new(0));
            let processor = QueuedSpanProcessor::spawn(RecordingExporter {
                spans: spans.clone(),
                shutdowns: shutdowns.clone(),
            })
            .expect("worker spawn");

            for _ in 0..5 {
                processor.on_end(sampled_span());
            }

            let t0 = Instant::now();
            let result = processor.shutdown();
            let elapsed = t0.elapsed();

            assert!(result.is_ok(), "clean drain must report Ok: {result:?}");
            assert!(
                elapsed < Duration::from_secs(2),
                "shutdown took {elapsed:?} — exceeds the 2s drain budget"
            );
            assert_eq!(
                spans.lock().unwrap_or_else(|e| e.into_inner()).len(),
                5,
                "all spans enqueued before shutdown must be exported by the drain"
            );
            assert_eq!(
                shutdowns.load(Ordering::SeqCst),
                1,
                "exporter shutdown exactly once"
            );
        }

        #[test]
        fn second_shutdown_is_prompt_err_not_hang() {
            let processor = QueuedSpanProcessor::spawn(RecordingExporter {
                spans: Arc::new(Mutex::new(Vec::new())),
                shutdowns: Arc::new(AtomicUsize::new(0)),
            })
            .expect("worker spawn");
            assert!(processor.shutdown().is_ok());
            // SpanProcessor contract: shutdown must be safe to call more
            // than once. Same shape as the SDK's BatchSpanProcessor:
            // prompt AlreadyShutdown error, no second drain.
            let t0 = Instant::now();
            assert!(processor.shutdown().is_err(), "second shutdown reports Err");
            assert!(t0.elapsed() < Duration::from_millis(500));
        }
    }
}
