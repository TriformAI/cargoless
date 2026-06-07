//! Transport abstraction (Model R #10, `D-FLEET-SHARED-DAEMON` §10).
//!
//! One logical API — [`VerdictService`] — bound to three interchangeable
//! transports from the same codebase:
//!
//! | adapter | module | use case (§10.2) |
//! |---|---|---|
//! | in-process | [`inproc`] | single-binary (`cargoless watch` — daemon + CLI in one process; zero IPC) |
//! | Unix socket | [`unix`] | local-default fleet (long-running `serve --repo` + many short CLI calls) |
//! | HTTP + SSE | [`http`] | network mode (`--bind <addr>`; cross-host orchestration) |
//!
//! Plus the CLI auto-discovery fallback chain ([`discovery`], §10.3):
//! `--remote <url>` → conventional Unix socket → file-read `cli-status` /
//! diagnostics (the v0 no-daemon behaviour) → spawn a local single-binary
//! daemon.
//!
//! ## Layering
//!
//! The logical API + DTOs live in `cargoless-core` and use **only**
//! core/proto types — the Stream-B serve loop (#3/#4) *implements*
//! [`VerdictService`] and the adapters are generic over it, so this seam
//! is definable without the serve-loop body in-tree (that is the point of
//! the abstraction). Per-crate verdicts are computed in the `cargoless`
//! cli crate (#9 `cratemap`), which `cargoless-core` cannot depend on;
//! the serve loop therefore passes already-rolled-up
//! [`CrateVerdict`]s into the status DTO. Diagnostics retention is
//! core-owned ([`crate::diagnostics_store`]) so [`VerdictService::
//! get_diagnostics`] can delegate directly.
//!
//! ## Auth seam (#14 — explicitly out of #10 scope)
//!
//! Network auth is Model R #14 (builder-infra), sequenced *after* #10.
//! This module defines the [`Authorizer`] seam + a default-permissive
//! [`AllowAll`]; the HTTP adapter consults it on every request. #14 swaps
//! a bearer-token `Authorizer` in **without reshaping the adapter** — the
//! seam is the contract, the policy is #14's.
//!
//! ## Dependency posture
//!
//! std-only + the crate's existing `serde_json` (Value + `json!`, no
//! derive — the sanctioned house tool; hand-rolled JSON for the wire is
//! the latent-bug factory the crate's dep rationale warns against). No
//! HTTP framework: the network adapter is a minimal, bounded HTTP/1.1 +
//! SSE over `std::net` (house ethos — JSON-RPC framing / debounce /
//! ignore are all hand-rolled in-crate already). Best-effort throughout:
//! a transport failure is surfaced as a typed error, never a panic.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use cargoless_proto::{Diagnostic, Severity};

use crate::batch::{BatchMember, BatchMemberResult, BatchProvenance, BatchReport, BatchVerdict};
use crate::config::{FleetConfig, FleetConfigError};

pub mod discovery;
pub mod http;
pub mod inproc;
pub mod unix;

/// One crate's verdict within a worktree (the #9 schema=2 `crates=`
/// roll-up, transport-DTO form). `verdict` is `"green"|"red"|"unknown"`
/// — string, not an enum, so the wire is forward-compatible and a
/// schema=1-era reader is unaffected (same discipline as cli-status #9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateVerdict {
    pub name: String,
    pub verdict: String,
}

/// Cargo subcommand attached to a pushed overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CargoSubcommand {
    #[default]
    Check,
    Clippy,
}

impl CargoSubcommand {
    pub fn as_str(self) -> &'static str {
        match self {
            CargoSubcommand::Check => "check",
            CargoSubcommand::Clippy => "clippy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "check" => Some(CargoSubcommand::Check),
            "clippy" => Some(CargoSubcommand::Clippy),
            _ => None,
        }
    }
}

/// Cargo profile attached to a pushed overlay.
///
/// This is intentionally the same small surface as the tf-multiverse
/// `check-remote` entrypoint: subcommand, package, target, features,
/// no-default-features, release, and a bounded set of extra Cargo selectors.
/// The serve daemon can keep one repo-scoped RA alive while each pushed
/// request still proves its final green with the exact Cargo selector the
/// caller asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CheckProfile {
    pub subcommand: CargoSubcommand,
    pub package: Option<String>,
    pub target: Option<String>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub release: bool,
    pub extra_args: Vec<String>,
}

impl CheckProfile {
    pub fn is_empty(&self) -> bool {
        self.subcommand == CargoSubcommand::Check
            && self.package.as_deref().unwrap_or("").trim().is_empty()
            && self.target.as_deref().unwrap_or("").trim().is_empty()
            && self.features.is_empty()
            && !self.no_default_features
            && !self.release
            && self.extra_args.is_empty()
    }
}

/// Optional metadata for a pushed overlay.
///
/// The original push wire carried client-local absolute paths, which is
/// correct for a same-host daemon. A central cluster daemon needs a different
/// shape: the client sends repo-relative paths and the server maps them onto
/// its own mirror root before rust-analyzer sees them. These fields are
/// additive and absent on old/local clients.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PushOverlayOptions {
    /// `true` means `files[*].0` are repo-relative paths. The receiving daemon
    /// must join them under `analysis_root` before applying the overlay.
    pub repo_relative: bool,
    /// Server-side repository root to analyze, for example
    /// `/workspace/tf-multiverse` in the cluster daemon pod.
    pub analysis_root: Option<String>,
    /// Client's resolved base SHA, diagnostics-only. The server still fetches
    /// its own latest base ref before accepting central-mode pushes.
    pub base_sha: Option<String>,
    /// Repo-relative files changed by the client diff. These are distinct
    /// from `files`: the overlay payload also includes workspace config files
    /// needed for cluster routing, while project-check triggers should see
    /// only the user diff.
    pub changed_files: Option<Vec<String>>,
}

impl PushOverlayOptions {
    pub fn is_empty(&self) -> bool {
        !self.repo_relative
            && self
                .analysis_root
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
            && self.base_sha.as_deref().unwrap_or("").trim().is_empty()
            && self
                .changed_files
                .as_ref()
                .is_none_or(|files| files.is_empty())
    }
}

/// A native batch gate request.
///
/// All members share one base/check profile/options block. v1 is deliberately
/// shaped for the central daemon path: many submitter overlays over one
/// server-side analysis root. Same-host multi-root batching can be layered on
/// later without changing the attribution report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCheckRequest {
    pub batch_id: String,
    pub base_ref: String,
    pub members: Vec<BatchMember>,
    pub check_profile: Option<CheckProfile>,
    pub options: PushOverlayOptions,
    /// `true` (default) checks the union first and only falls back to solos on
    /// a combined red. `false` forces solo checks, useful for debugging and
    /// for wrappers that need per-submit proof.
    pub corun: bool,
}

impl BatchCheckRequest {
    #[must_use]
    pub fn new(batch_id: impl Into<String>, base_ref: impl Into<String>) -> Self {
        Self {
            batch_id: batch_id.into(),
            base_ref: base_ref.into(),
            members: Vec::new(),
            check_profile: None,
            options: PushOverlayOptions::default(),
            corun: true,
        }
    }
}

/// Transport-agnostic worktree status (the `get_status` payload, §10.1).
/// `crates` empty ⇒ no trustworthy per-crate breakdown (single-crate, or
/// the #9 unattributable-error honesty case); `verdict` is **always** the
/// authoritative tree verdict and stands alone — the sidecar discipline
/// (#11/#176) carried into the transport layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub worktree: String,
    pub verdict: String,
    pub daemon_build_id: String,
    pub crates: Vec<CrateVerdict>,
    pub red_diagnostics: u32,
    /// **INFRA-36** — non-empty iff `verdict == "unknown"`. Stable
    /// classifier string (e.g., `"project_check_setup_error: <detail>"`,
    /// `"ra_native_unattributed_error"`) that distinguishes a real
    /// gate-failed Red from a daemon-couldn't-evaluate Unknown. SigNoz
    /// dashboards group on the leading classifier (the substring before
    /// `: `) to separate "gate doing its job" from "gate didn't run".
    pub verdict_failure_reason: Option<String>,
    pub heartbeat_age_secs: u64,
    pub published_at: u64,
}

/// Light per-worktree summary for `list_worktrees` (§10.1) — just enough
/// for a dashboard without the heavy diagnostics payload (asymmetric
/// principle: terse by default, detail on demand via `get_diagnostics`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSummary {
    pub worktree: String,
    pub verdict: String,
    pub daemon_build_id: String,
    pub red_diagnostics: u32,
}

/// A verdict-transition event for `subscribe` (§10.1, SSE-style stream).
/// Carries the new status; the HTTP adapter renders it as an SSE
/// `data:` frame, the Unix adapter as a newline-delimited JSON record,
/// the in-proc adapter hands the `Receiver` back directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionEvent {
    pub worktree: String,
    pub verdict: String,
    pub red_diagnostics: u32,
    /// **INFRA-36** — see [`WorktreeStatus::verdict_failure_reason`]
    /// for the contract. Mirrored on transition events so SSE
    /// subscribers can distinguish a real Red transition from an
    /// Unknown-the-daemon-couldn't-evaluate without round-tripping a
    /// `get_status` call.
    pub verdict_failure_reason: Option<String>,
    pub published_at: u64,
}

/// Daemon drain/quiesce state exposed through the admin HTTP surface.
///
/// `active_worktrees` counts worktrees with an accepted pushed overlay whose
/// fresh verdict has not yet been published. `pending_pushes` is the subset
/// still waiting in the overlay store. A restart is drain-safe once
/// `quiescing == true` and both counts reach zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonActivity {
    pub quiescing: bool,
    pub active_worktrees: u32,
    pub pending_pushes: u32,
}

fn unsupported_batch_report(
    request: &BatchCheckRequest,
    message: impl Into<String>,
) -> BatchReport {
    let message = message.into();
    BatchReport {
        batch_id: request.batch_id.clone(),
        verdict: BatchVerdict::Indeterminate,
        members: request
            .members
            .iter()
            .map(|member| BatchMemberResult {
                worktree: member.worktree.clone(),
                verdict: BatchVerdict::Indeterminate,
                provenance: BatchProvenance::Indeterminate,
                diagnostics: vec![Diagnostic {
                    file_path: PathBuf::from("<cargoless-batch>"),
                    line: 0,
                    col: 0,
                    severity: Severity::Error,
                    code: Some("cargoless.batch.unsupported".into()),
                    message: message.clone(),
                    source: Some("cargoless".into()),
                }],
                duration_ms: 0,
            })
            .collect(),
        combined_checks: 0,
        solo_checks: 0,
        duration_ms: 0,
    }
}

/// The single logical API (§10.1). The Stream-B serve loop implements
/// this; every adapter is generic over `S: VerdictService`. `Send +
/// Sync` so the Unix/HTTP adapters can share one service across
/// connection threads.
pub trait VerdictService: Send + Sync {
    /// Full status for a worktree (current verdict + heartbeat +
    /// per-crate breakdown). `None` ⇒ unknown worktree.
    fn get_status(&self, worktree: &str) -> Option<WorktreeStatus>;

    /// Just the verdict string (light — no per-crate, no heartbeat).
    /// `None` ⇒ unknown worktree.
    fn get_verdict(&self, worktree: &str) -> Option<String>;

    /// Full retained diagnostics for a worktree's current red state
    /// (heavy). Empty ⇒ green / never-red / unknown (callers treat "no
    /// detail" and "green" the same — correct, a green tree retains
    /// nothing; see [`crate::diagnostics_store`]).
    fn get_diagnostics(&self, worktree: &str) -> Vec<Diagnostic>;

    /// All discovered worktrees with their light verdict summary.
    fn list_worktrees(&self) -> Vec<WorktreeSummary>;

    /// Subscribe to the transition-event stream. The serve loop owns the
    /// fan-out; each call yields an independent `Receiver`.
    fn subscribe(&self) -> Receiver<TransitionEvent>;

    /// Increment 2 (D-PUSHOVERLAY §2.4) — ingest a pushed overlay-set for
    /// `worktree`. ADDITIVE with a **default body** so no existing impl
    /// is forced to change (the v0.2.0 `MockService` and the in-proc /
    /// Unix / HTTP read paths all keep compiling untouched). The real
    /// implementor is the serve loop's `VerdictService`, which overrides
    /// this to feed the per-WT overlay store; the default is an honest
    /// refusal (`accepted: false`) — a service that has not opted into
    /// push-ingest reports it stored nothing.
    fn push_overlay(
        &self,
        worktree: &str,
        _base_ref: &str,
        _files: &[(String, String)],
    ) -> PushOverlayAck {
        PushOverlayAck {
            worktree: worktree.to_string(),
            accepted: false,
            applied_files: 0,
        }
    }

    /// Increment 3 — same write-ingest path with a per-request cargo check
    /// profile. Default delegates to the profile-less verb so older/mock
    /// services fail closed or accept exactly as before.
    fn push_overlay_with_profile(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        _check_profile: Option<&CheckProfile>,
    ) -> PushOverlayAck {
        self.push_overlay(worktree, base_ref, files)
    }

    /// Increment 4 — push overlay plus optional central-daemon mapping
    /// metadata. Default delegates to the existing profile-aware path so old
    /// services remain source-compatible and keep their current behavior.
    fn push_overlay_with_options(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&CheckProfile>,
        _options: Option<&PushOverlayOptions>,
    ) -> PushOverlayAck {
        self.push_overlay_with_profile(worktree, base_ref, files, check_profile)
    }

    /// Native batch gate: check several pushed overlays together and return
    /// per-member attribution. Default is an honest indeterminate report so
    /// older/mock services remain source-compatible but never claim green.
    fn batch_check(&self, request: &BatchCheckRequest) -> BatchReport {
        unsupported_batch_report(request, "batch_check unsupported by this service")
    }

    /// Admin read: current drain/quiesce state. Default keeps older/mock
    /// services source-compatible and reports "idle, not quiescing".
    fn daemon_activity(&self) -> DaemonActivity {
        DaemonActivity::default()
    }

    /// Admin write: enter quiesce mode. A real daemon refuses subsequent
    /// pushes, drains accepted work, then exits from its serve loop. Default
    /// is a no-op so non-daemon test services keep compiling.
    fn request_quiesce(&self) -> DaemonActivity {
        self.daemon_activity()
    }
}

/// The **client** counterpart of [`VerdictService`] — the uniform
/// surface the CLI programs against regardless of which transport
/// [`discovery`] resolved. Every adapter ships a client implementing
/// this; the CLI fallback chain swaps implementations without changing
/// call sites. Methods return [`TransportError`] (in-proc is infallible
/// and always `Ok`, but the signature is uniform so a fallible socket /
/// HTTP client is a drop-in).
pub trait TransportClient {
    fn get_status(&self, worktree: &str) -> Result<Option<WorktreeStatus>, TransportError>;
    fn get_verdict(&self, worktree: &str) -> Result<Option<String>, TransportError>;
    fn get_diagnostics(&self, worktree: &str) -> Result<Vec<Diagnostic>, TransportError>;
    fn list_worktrees(&self) -> Result<Vec<WorktreeSummary>, TransportError>;
    /// Subscribe to transitions. Returns a `Receiver` so all three
    /// transports present the same pull interface (in-proc hands the
    /// service receiver back; Unix/HTTP spawn a reader thread that
    /// forwards decoded frames into a channel).
    fn subscribe(&self) -> Result<Receiver<TransitionEvent>, TransportError>;

    /// Increment 2 — push an overlay-set to the daemon. ADDITIVE with a
    /// default `Err` so existing clients/call-sites are unaffected; the
    /// real implementors (`HttpClient`, `UnixClient`, `InProcClient`)
    /// override it. Write-only: the verdict is NOT in the ack — the
    /// caller then polls `get_status` / `subscribe` for it.
    fn push_overlay(
        &self,
        _worktree: &str,
        _base_ref: &str,
        _files: &[(String, String)],
    ) -> Result<PushOverlayAck, TransportError> {
        Err(TransportError::Protocol(
            "push_overlay unsupported by this transport".into(),
        ))
    }

    /// Increment 3 — push overlay plus exact cargo-check profile. Default
    /// delegates to the profile-less method for existing clients.
    fn push_overlay_with_profile(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        _check_profile: Option<&CheckProfile>,
    ) -> Result<PushOverlayAck, TransportError> {
        self.push_overlay(worktree, base_ref, files)
    }

    /// Increment 4 — push overlay plus optional central-daemon mapping
    /// metadata. Default delegates to the existing profile-aware path for
    /// transports that have not opted into the extended wire yet.
    fn push_overlay_with_options(
        &self,
        worktree: &str,
        base_ref: &str,
        files: &[(String, String)],
        check_profile: Option<&CheckProfile>,
        _options: Option<&PushOverlayOptions>,
    ) -> Result<PushOverlayAck, TransportError> {
        self.push_overlay_with_profile(worktree, base_ref, files, check_profile)
    }

    /// Native batch gate. Default is unsupported for transports that have not
    /// opted into the batch wire yet.
    fn batch_check(&self, _request: &BatchCheckRequest) -> Result<BatchReport, TransportError> {
        Err(TransportError::Protocol(
            "batch_check unsupported by this transport".into(),
        ))
    }
}

/// Network-auth seam (Model R #14 — NOT implemented in #10). The HTTP
/// adapter calls [`Authorizer::authorize`] on every request with the
/// presented bearer token (if any). #14 provides a real token policy by
/// swapping the `Arc<dyn Authorizer>` — the adapter is unchanged.
pub trait Authorizer: Send + Sync {
    /// `true` ⇒ allow. `token` is the `Authorization: Bearer <token>`
    /// value if the client sent one, else `None`.
    fn authorize(&self, token: Option<&str>) -> bool;
}

/// Default-permissive authorizer (the #10 posture: localhost-only,
/// no auth — `D-FLEET §10.4`). #14 replaces this with a bearer-token
/// policy for `--bind`-to-network deployments. Named (not a closure) so
/// the "this is intentionally open in #10" decision is greppable.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _token: Option<&str>) -> bool {
        true
    }
}

/// #14 — bearer-token [`Authorizer`] for network (`--bind`) mode.
///
/// Allows a request iff it presents `Authorization: Bearer <token>` whose
/// value equals the configured secret. A request with no token is denied
/// (⇒ the HTTP adapter's existing clean `401`); the #10 seam is unchanged
/// — this is pure policy swapped in via [`authorizer_for`].
///
/// ## Constant-time content compare (the load-bearing security property)
///
/// The token compare must not early-return on the first differing byte —
/// that leaks, via response timing, a prefix-matching oracle that turns
/// secret recovery from `O(charset^len)` into `O(charset*len)`. The
/// content comparison here folds every byte into a single accumulator
/// with `|=` and only inspects the accumulator at the end: the work is
/// independent of *where* (or whether) a mismatch occurs.
///
/// Length is compared first and may short-circuit: this is the standard
/// token-compare discipline (ring `verify_slices_are_equal`, OpenSSL
/// `CRYPTO_memcmp` both require equal length). A bearer token's *length*
/// is low-entropy and not the secret; its *content* is. Equalising the
/// loop bound on unequal lengths would compare against attacker-chosen
/// bytes and still reveal nothing the length didn't — the standard
/// trade, made explicit.
pub struct BearerToken {
    secret: Vec<u8>,
}

impl BearerToken {
    /// The configured shared secret (from `--auth-token` /
    /// `CARGOLESS_AUTH_TOKEN` / `tf.toml [fleet] auth_token`, resolved
    /// through the frozen #1 `FleetConfig` contract).
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into().into_bytes(),
        }
    }
}

/// Constant-time-content byte-slice equality (see [`BearerToken`] docs).
/// `#[inline(never)]` so an optimiser can't peel the loop into an
/// early-exit shape that reintroduces the timing oracle.
#[inline(never)]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl Authorizer for BearerToken {
    fn authorize(&self, token: Option<&str>) -> bool {
        // Consumer-side make-the-bad-state-unrepresentable (CWDL #197):
        // a BearerToken whose configured secret is empty or
        // whitespace-only authorizes NOTHING. Even if some future config
        // path reconstructed an empty-secret bearer, it fails CLOSED
        // (deny → 401) — never an empty-`Authorization: Bearer ` bypass.
        // `[].iter().all(..)` is vacuously true ⇒ an empty secret denies.
        if self.secret.iter().all(u8::is_ascii_whitespace) {
            return false;
        }
        match token {
            // No credential presented ⇒ deny (HTTP adapter → 401).
            None => false,
            Some(presented) => constant_time_eq(presented.as_bytes(), &self.secret),
        }
    }
}

/// Select the network [`Authorizer`] for a resolved [`FleetConfig`],
/// **failing closed**.
///
/// This is the #14 policy seam binding (the HTTP adapter takes the
/// returned `Arc<dyn Authorizer>` unchanged — `D-FLEET §10.4`):
///
/// * non-loopback `bind` **without** an `auth_token` ⇒ `Err` (the
///   [`FleetConfig::security_check`] by-construction refusal — the
///   daemon must NOT serve an unauthenticated socket reachable
///   off-host; surfacing the typed config error is the safe failure,
///   never a silent [`AllowAll`] on a public bind);
/// * an `auth_token` present ⇒ [`BearerToken`] (enforced even on a
///   loopback bind — opting into auth is always honoured);
/// * otherwise (no token; absent or loopback `bind`) ⇒ [`AllowAll`],
///   the #10 localhost-only posture, unchanged.
///
/// Pure: no I/O, no socket — the serve/daemon I/O-shell calls this and
/// hands the result to `HttpServer::bind`. Exhaustively unit-tested over
/// the loopback/non-loopback × token/no-token matrix.
pub fn authorizer_for(cfg: &FleetConfig) -> Result<Arc<dyn Authorizer>, FleetConfigError> {
    // Fail closed first: a network-reachable bind with no token is
    // refused here, not downgraded to permissive.
    cfg.security_check()?;
    // Single source of truth for "an effective secret exists" (CWDL
    // #197): a blank (empty / whitespace-only) configured token is NOT a
    // token — `effective_auth_token()` returns `None`, so a blank token
    // yields `AllowAll` only where `security_check` already permits it
    // (loopback / no bind); a non-loopback blank token was refused by
    // `security_check` above. No `BearerToken` is ever built from a
    // blank secret.
    Ok(match cfg.effective_auth_token() {
        Some(secret) => Arc::new(BearerToken::new(secret)),
        None => Arc::new(AllowAll),
    })
}

/// A transport error. Best-effort discipline: adapters return this, never
/// panic; the CLI fallback chain ([`discovery`]) treats any `Err` as
/// "this transport unavailable, try the next".
#[derive(Debug)]
pub enum TransportError {
    /// Socket/TCP/HTTP I/O failure (connection refused, reset, timeout).
    Io(std::io::Error),
    /// Wire payload could not be parsed (malformed JSON / framing).
    Protocol(String),
    /// Auth denied by the [`Authorizer`] (#14; never produced under
    /// [`AllowAll`]). Defined now so #14 adds policy, not a new variant.
    Unauthorized,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "transport I/O: {e}"),
            TransportError::Protocol(m) => write!(f, "transport protocol: {m}"),
            TransportError::Unauthorized => write!(f, "transport: unauthorized"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e)
    }
}

// --------------------------------------------------------------------------
// Wire codec — one place, shared by the Unix + HTTP adapters so the two
// transports speak byte-identical JSON (serde_json::Value, no derive —
// house style, cf. `diagnostics_store`). Pure (no I/O) ⇒ unit-tested
// directly without a socket.
// --------------------------------------------------------------------------

/// The set of logical calls, as a parsed request (the JSON-RPC-ish
/// envelope the Unix/HTTP adapters decode a line / request body into).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    GetStatus(String),
    GetVerdict(String),
    GetDiagnostics(String),
    ListWorktrees,
    Subscribe,
    /// Increment 2 (D-PUSHOVERLAY §2) — ADDITIVE write-ingest verb. The
    /// five variants above are byte-frozen; this is appended last. The
    /// thin push-client sends whole-file `(path, content)` pairs (never a
    /// keystroke diff — the client owns its overlay-set). Write-only: it
    /// does NOT carry a verdict back; the verdict reaches the client via
    /// the already-shipped `subscribe`/`get_status` read plane.
    PushOverlay {
        worktree: String,
        base_ref: String,
        files: Vec<(String, String)>,
        check_profile: Option<CheckProfile>,
    },
    /// Increment 4 — central-daemon push. Same operation as PushOverlay, with
    /// additive metadata that lets a remote daemon map repo-relative overlays
    /// onto its own checked-out mirror root.
    PushOverlayV2 {
        worktree: String,
        base_ref: String,
        files: Vec<(String, String)>,
        check_profile: Option<CheckProfile>,
        options: PushOverlayOptions,
    },
    /// Native batch gate. The request checks several submitter overlays as
    /// one candidate merge set, then falls back to solo checks only when the
    /// combined set is red.
    BatchCheck(BatchCheckRequest),
}

impl Request {
    /// Parse `{"op":"get_status","worktree":"W"}` (best-effort; unknown
    /// op ⇒ `None` so the adapter answers a clean protocol error rather
    /// than panicking).
    pub fn from_json(text: &str) -> Option<Request> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let op = v.get("op")?.as_str()?;
        let wt = || {
            v.get("worktree")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match op {
            "get_status" => Some(Request::GetStatus(wt())),
            "get_verdict" => Some(Request::GetVerdict(wt())),
            "get_diagnostics" => Some(Request::GetDiagnostics(wt())),
            "list_worktrees" => Some(Request::ListWorktrees),
            "subscribe" => Some(Request::Subscribe),
            // Increment 2: best-effort (mirrors the rules above) — a
            // missing/`!array` `files` ⇒ empty vec, a malformed element
            // (no `path`) is skipped; never a panic. `base_ref` absent ⇒
            // empty string (same posture as `wt()`).
            "push_overlay" => {
                let worktree = wt();
                let base_ref = v
                    .get("base_ref")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let files = overlay_files_from_json(v.get("files"));
                let check_profile = check_profile_from_json(v.get("check_profile"));
                let options = push_overlay_options_from_json(&v);
                if options.is_empty() {
                    Some(Request::PushOverlay {
                        worktree,
                        base_ref,
                        files,
                        check_profile,
                    })
                } else {
                    Some(Request::PushOverlayV2 {
                        worktree,
                        base_ref,
                        files,
                        check_profile,
                        options,
                    })
                }
            }
            "batch_check" => Some(Request::BatchCheck(batch_check_request_from_value(&v))),
            _ => None,
        }
    }

    pub fn to_json(&self) -> String {
        let v = match self {
            Request::GetStatus(w) => serde_json::json!({"op":"get_status","worktree":w}),
            Request::GetVerdict(w) => serde_json::json!({"op":"get_verdict","worktree":w}),
            Request::GetDiagnostics(w) => {
                serde_json::json!({"op":"get_diagnostics","worktree":w})
            }
            Request::ListWorktrees => serde_json::json!({"op":"list_worktrees"}),
            Request::Subscribe => serde_json::json!({"op":"subscribe"}),
            Request::PushOverlay {
                worktree,
                base_ref,
                files,
                check_profile,
            } => serde_json::json!({
                "op": "push_overlay",
                "worktree": worktree,
                "base_ref": base_ref,
                "files": overlay_files_to_json(files),
                "check_profile": check_profile_json(check_profile.as_ref()),
            }),
            Request::PushOverlayV2 {
                worktree,
                base_ref,
                files,
                check_profile,
                options,
            } => {
                let mut v = serde_json::json!({
                    "op": "push_overlay",
                    "worktree": worktree,
                    "base_ref": base_ref,
                    "files": overlay_files_to_json(files),
                    "check_profile": check_profile_json(check_profile.as_ref()),
                    "repo_relative": options.repo_relative,
                });
                if let serde_json::Value::Object(ref mut map) = v {
                    if let Some(root) = options.analysis_root.as_deref() {
                        map.insert(
                            "analysis_root".to_string(),
                            serde_json::Value::String(root.to_string()),
                        );
                    }
                    if let Some(sha) = options.base_sha.as_deref() {
                        map.insert(
                            "base_sha".to_string(),
                            serde_json::Value::String(sha.to_string()),
                        );
                    }
                    if let Some(changed_files) = options
                        .changed_files
                        .as_ref()
                        .filter(|files| !files.is_empty())
                    {
                        map.insert(
                            "changed_files".to_string(),
                            serde_json::Value::Array(
                                changed_files
                                    .iter()
                                    .map(|path| serde_json::Value::String(path.clone()))
                                    .collect(),
                            ),
                        );
                    }
                }
                v
            }
            Request::BatchCheck(request) => batch_check_request_to_json(request),
        };
        v.to_string()
    }
}

fn batch_check_request_to_json(request: &BatchCheckRequest) -> serde_json::Value {
    serde_json::json!({
        "op": "batch_check",
        "batch_id": request.batch_id,
        "base_ref": request.base_ref,
        "members": batch_members_to_json(&request.members),
        "check_profile": check_profile_json(request.check_profile.as_ref()),
        "options": push_overlay_options_to_json(&request.options),
        "corun": request.corun,
    })
}

fn batch_check_request_from_value(v: &serde_json::Value) -> BatchCheckRequest {
    BatchCheckRequest {
        batch_id: v
            .get("batch_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_ref: v
            .get("base_ref")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        members: batch_members_from_json(v.get("members")),
        check_profile: check_profile_from_json(v.get("check_profile")),
        options: push_overlay_options_from_json(v.get("options").unwrap_or(v)),
        corun: v
            .get("corun")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
    }
}

fn batch_members_to_json(members: &[BatchMember]) -> serde_json::Value {
    serde_json::Value::Array(
        members
            .iter()
            .map(|member| {
                serde_json::json!({
                    "worktree": member.worktree,
                    "files": overlay_files_to_json(&member.files),
                    "changed_files": member.changed_files,
                })
            })
            .collect(),
    )
}

fn batch_members_from_json(v: Option<&serde_json::Value>) -> Vec<BatchMember> {
    let Some(serde_json::Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(BatchMember {
                worktree: item.get("worktree")?.as_str()?.to_string(),
                files: overlay_files_from_json(item.get("files")),
                changed_files: string_array_from_json(item.get("changed_files"))
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn push_overlay_options_to_json(options: &PushOverlayOptions) -> serde_json::Value {
    let mut value = serde_json::json!({
        "repo_relative": options.repo_relative,
        "gate": options.gate,
    });
    if let serde_json::Value::Object(ref mut map) = value {
        if let Some(root) = options.analysis_root.as_deref() {
            map.insert(
                "analysis_root".to_string(),
                serde_json::Value::String(root.to_string()),
            );
        }
        if let Some(sha) = options.base_sha.as_deref() {
            map.insert(
                "base_sha".to_string(),
                serde_json::Value::String(sha.to_string()),
            );
        }
        if let Some(changed_files) = options
            .changed_files
            .as_ref()
            .filter(|files| !files.is_empty())
        {
            map.insert(
                "changed_files".to_string(),
                serde_json::Value::Array(
                    changed_files
                        .iter()
                        .map(|path| serde_json::Value::String(path.clone()))
                        .collect(),
                ),
            );
        }
    }
    value
}

fn push_overlay_options_from_json(v: &serde_json::Value) -> PushOverlayOptions {
    PushOverlayOptions {
        repo_relative: v
            .get("repo_relative")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        analysis_root: v
            .get("analysis_root")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty()),
        base_sha: v
            .get("base_sha")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty()),
        changed_files: string_array_from_json(v.get("changed_files")),
    }
}

fn string_array_from_json(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
    v.and_then(serde_json::Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn check_profile_json(profile: Option<&CheckProfile>) -> serde_json::Value {
    let Some(profile) = profile else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "subcommand": profile.subcommand.as_str(),
        "package": profile.package.as_deref(),
        "target": profile.target.as_deref(),
        "features": &profile.features,
        "no_default_features": profile.no_default_features,
        "release": profile.release,
        "extra_args": &profile.extra_args,
    })
}

fn check_profile_from_json(v: Option<&serde_json::Value>) -> Option<CheckProfile> {
    let v = v?;
    if v.is_null() {
        return None;
    }
    let profile = CheckProfile {
        subcommand: v
            .get("subcommand")
            .and_then(serde_json::Value::as_str)
            .and_then(CargoSubcommand::parse)
            .unwrap_or_default(),
        package: v
            .get("package")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty()),
        target: v
            .get("target")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty()),
        features: v
            .get("features")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        no_default_features: v
            .get("no_default_features")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        release: v
            .get("release")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        extra_args: v
            .get("extra_args")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    };
    (!profile.is_empty()).then_some(profile)
}

/// Serialise the `PushOverlay` `files` payload as a JSON array of
/// `{"path":..,"content":..}` objects (Increment 2). Hand-rolled `Value`,
/// no derive — same house style as `crate_verdicts_json`.
fn overlay_files_to_json(files: &[(String, String)]) -> serde_json::Value {
    serde_json::Value::Array(
        files
            .iter()
            .map(|(p, c)| serde_json::json!({"path": p, "content": c}))
            .collect(),
    )
}

/// Parse the `PushOverlay` `files` array (Increment 2). Best-effort,
/// mirroring `crate_verdicts_from_json` exactly: a non-array (or `None`)
/// ⇒ empty vec; an element with no `path` is skipped (not fatal); a
/// missing `content` defaults to empty string. Never panics.
fn overlay_files_from_json(v: Option<&serde_json::Value>) -> Vec<(String, String)> {
    let Some(serde_json::Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|f| {
            let path = f.get("path")?.as_str()?.to_string();
            let content = f
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((path, content))
        })
        .collect()
}

/// Increment 2 (D-PUSHOVERLAY §2.3) — the cheap write-ingest ack for
/// [`Request::PushOverlay`]. `PushOverlay` does NOT block on a verdict;
/// the client obtains the verdict via the already-shipped
/// `subscribe`/`get_status` read plane. `accepted` is the server's
/// "stored it" signal; `applied_files` is the count it persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOverlayAck {
    pub worktree: String,
    pub accepted: bool,
    pub applied_files: u32,
}

/// Serialise a [`PushOverlayAck`] to wire JSON
/// (`{"worktree":..,"accepted":..,"applied_files":..}`).
pub fn pushoverlayack_to_json(a: &PushOverlayAck) -> String {
    serde_json::json!({
        "worktree": a.worktree,
        "accepted": a.accepted,
        "applied_files": a.applied_files,
    })
    .to_string()
}

/// Parse a [`PushOverlayAck`] from wire JSON (best-effort: a missing
/// `worktree` ⇒ `None`; missing scalars default to `false`/`0`, never a
/// panic — same posture as [`status_from_json`]).
pub fn pushoverlayack_from_json(text: &str) -> Option<PushOverlayAck> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(PushOverlayAck {
        worktree: v.get("worktree")?.as_str()?.to_string(),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        applied_files: v
            .get("applied_files")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

/// Serialise a native batch attribution report to wire JSON.
pub fn batchreport_to_json(report: &BatchReport) -> String {
    serde_json::json!({
        "batch_id": report.batch_id,
        "verdict": report.verdict.as_str(),
        "members": report.members.iter().map(batch_member_result_to_json).collect::<Vec<_>>(),
        "combined_checks": report.combined_checks,
        "solo_checks": report.solo_checks,
        "duration_ms": report.duration_ms.to_string(),
    })
    .to_string()
}

/// Parse a native batch attribution report from wire JSON. Best-effort:
/// malformed member rows are skipped, and unknown verdict/provenance strings
/// become indeterminate.
pub fn batchreport_from_json(text: &str) -> Option<BatchReport> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(BatchReport {
        batch_id: v.get("batch_id")?.as_str()?.to_string(),
        verdict: v
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .and_then(BatchVerdict::parse)
            .unwrap_or(BatchVerdict::Indeterminate),
        members: batch_member_results_from_json(v.get("members")),
        combined_checks: v
            .get("combined_checks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        solo_checks: v
            .get("solo_checks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        duration_ms: json_u128(v.get("duration_ms")),
    })
}

fn batch_member_result_to_json(result: &BatchMemberResult) -> serde_json::Value {
    serde_json::json!({
        "worktree": result.worktree,
        "verdict": result.verdict.as_str(),
        "provenance": result.provenance.as_str(),
        "diagnostics": diagnostics_to_json(&result.diagnostics),
        "duration_ms": result.duration_ms.to_string(),
    })
}

fn batch_member_results_from_json(v: Option<&serde_json::Value>) -> Vec<BatchMemberResult> {
    let Some(serde_json::Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(BatchMemberResult {
                worktree: item.get("worktree")?.as_str()?.to_string(),
                verdict: item
                    .get("verdict")
                    .and_then(serde_json::Value::as_str)
                    .and_then(BatchVerdict::parse)
                    .unwrap_or(BatchVerdict::Indeterminate),
                provenance: item
                    .get("provenance")
                    .and_then(serde_json::Value::as_str)
                    .and_then(BatchProvenance::parse)
                    .unwrap_or(BatchProvenance::Indeterminate),
                diagnostics: diagnostics_from_value(item.get("diagnostics")),
                duration_ms: json_u128(item.get("duration_ms")),
            })
        })
        .collect()
}

fn diagnostics_to_json(diags: &[Diagnostic]) -> serde_json::Value {
    serde_json::from_str(&crate::diagnostics_store::serialize(diags))
        .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
}

fn diagnostics_from_value(v: Option<&serde_json::Value>) -> Vec<Diagnostic> {
    v.map(serde_json::Value::to_string)
        .map(|text| crate::diagnostics_store::deserialize(&text))
        .unwrap_or_default()
}

fn json_u128(v: Option<&serde_json::Value>) -> u128 {
    v.and_then(serde_json::Value::as_u64)
        .map(u128::from)
        .or_else(|| {
            v.and_then(serde_json::Value::as_str)
                .and_then(|s| s.parse::<u128>().ok())
        })
        .unwrap_or(0)
}

fn crate_verdicts_json(crates: &[CrateVerdict]) -> serde_json::Value {
    serde_json::Value::Array(
        crates
            .iter()
            .map(|c| serde_json::json!({"name": c.name, "verdict": c.verdict}))
            .collect(),
    )
}

fn crate_verdicts_from_json(v: Option<&serde_json::Value>) -> Vec<CrateVerdict> {
    let Some(serde_json::Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|c| {
            Some(CrateVerdict {
                name: c.get("name")?.as_str()?.to_string(),
                verdict: c
                    .get("verdict")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
            })
        })
        .collect()
}

/// Serialise a `WorktreeStatus` to the wire JSON.
///
/// **INFRA-36:** emits `verdict_failure_reason` only when populated, so
/// pre-INFRA-36 clients parsing this output see no new key (the absent
/// case is `None`, which `serde_json::Value::Null` would also represent
/// — omitting is cleaner and stays additive on the wire).
pub fn status_to_json(s: &WorktreeStatus) -> String {
    let mut obj = serde_json::json!({
        "worktree": s.worktree,
        "verdict": s.verdict,
        "daemon_build_id": s.daemon_build_id,
        "crates": crate_verdicts_json(&s.crates),
        "red_diagnostics": s.red_diagnostics,
        "heartbeat_age_secs": s.heartbeat_age_secs,
        "published_at": s.published_at,
    });
    if let Some(reason) = &s.verdict_failure_reason {
        obj.as_object_mut()
            .expect("status_to_json constructed an object literal")
            .insert(
                "verdict_failure_reason".to_string(),
                serde_json::Value::String(reason.clone()),
            );
    }
    obj.to_string()
}

/// Parse wire JSON back to a `WorktreeStatus` (best-effort: a missing
/// `worktree` ⇒ `None`; missing scalars ⇒ 0/empty, never a panic).
///
/// **INFRA-36:** `verdict_failure_reason` is optional on the wire and
/// `None` when absent — backward-compatible with pre-INFRA-36 servers
/// that never emit the key.
pub fn status_from_json(text: &str) -> Option<WorktreeStatus> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(WorktreeStatus {
        worktree: v.get("worktree")?.as_str()?.to_string(),
        verdict: v
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        daemon_build_id: v
            .get("daemon_build_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        crates: crate_verdicts_from_json(v.get("crates")),
        red_diagnostics: v
            .get("red_diagnostics")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        verdict_failure_reason: v
            .get("verdict_failure_reason")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.to_string()),
        heartbeat_age_secs: v
            .get("heartbeat_age_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        published_at: v
            .get("published_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    })
}

/// Serialise the `list_worktrees` summary array.
pub fn summaries_to_json(list: &[WorktreeSummary]) -> String {
    serde_json::Value::Array(
        list.iter()
            .map(|s| {
                serde_json::json!({
                    "worktree": s.worktree,
                    "verdict": s.verdict,
                    "daemon_build_id": s.daemon_build_id,
                    "red_diagnostics": s.red_diagnostics,
                })
            })
            .collect(),
    )
    .to_string()
}

/// Parse the `list_worktrees` summary array (best-effort, skips malformed
/// elements — a dashboard degrades to fewer rows, never crashes).
pub fn summaries_from_json(text: &str) -> Vec<WorktreeSummary> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|s| {
            Some(WorktreeSummary {
                worktree: s.get("worktree")?.as_str()?.to_string(),
                verdict: s
                    .get("verdict")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                daemon_build_id: s
                    .get("daemon_build_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                red_diagnostics: s
                    .get("red_diagnostics")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
            })
        })
        .collect()
}

/// Serialise a transition event (SSE `data:` payload / Unix NDJSON line).
///
/// **INFRA-36:** `verdict_failure_reason` is emitted only when populated
/// — pre-INFRA-36 clients see no new key, additive forward-compat.
pub fn event_to_json(e: &TransitionEvent) -> String {
    let mut obj = serde_json::json!({
        "worktree": e.worktree,
        "verdict": e.verdict,
        "red_diagnostics": e.red_diagnostics,
        "published_at": e.published_at,
    });
    if let Some(reason) = &e.verdict_failure_reason {
        obj.as_object_mut()
            .expect("event_to_json constructed an object literal")
            .insert(
                "verdict_failure_reason".to_string(),
                serde_json::Value::String(reason.clone()),
            );
    }
    obj.to_string()
}

/// Parse a transition event from its wire JSON (the `subscribe` NDJSON
/// frame / SSE `data:` payload). Shared by the Unix + HTTP stream
/// clients so both decode byte-identically. Best-effort: a malformed
/// frame ⇒ `None` (the stream client skips it, never panics).
///
/// **INFRA-36:** `verdict_failure_reason` defaults to `None` when
/// absent — backward-compatible with pre-INFRA-36 servers.
pub fn event_from_json(text: &str) -> Option<TransitionEvent> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(TransitionEvent {
        worktree: v.get("worktree")?.as_str()?.to_string(),
        verdict: v
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        red_diagnostics: v
            .get("red_diagnostics")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        verdict_failure_reason: v
            .get("verdict_failure_reason")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.to_string()),
        published_at: v
            .get("published_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FleetConfig;

    // ───────────────────────── #14 auth ─────────────────────────

    #[test]
    fn bearer_token_accepts_exact_denies_wrong_and_none() {
        let a = BearerToken::new("s3cr3t-abc");
        assert!(a.authorize(Some("s3cr3t-abc")), "exact match ⇒ allow");
        assert!(!a.authorize(Some("s3cr3t-abd")), "1-byte-off ⇒ deny");
        assert!(!a.authorize(Some("s3cr3t-ab")), "prefix (shorter) ⇒ deny");
        assert!(
            !a.authorize(Some("s3cr3t-abcd")),
            "superstring (longer) ⇒ deny"
        );
        assert!(!a.authorize(Some("")), "empty presented ⇒ deny");
        assert!(!a.authorize(None), "no credential ⇒ deny (→ adapter 401)");
    }

    #[test]
    fn constant_time_eq_is_correct_total_and_length_safe() {
        // Correctness (the timing property itself is structural — no
        // early return over content — and asserted by code review, not a
        // flaky wall-clock test; here we pin the FUNCTIONAL contract).
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd")); // last byte differs
        assert!(!constant_time_eq(b"abc", b"Xbc")); // first byte differs
        assert!(!constant_time_eq(b"abc", b"ab")); // length differs
        assert!(!constant_time_eq(b"ab", b"abc"));
        // A first-byte mismatch and a last-byte mismatch are both `false`
        // — the accumulator folds the whole equal-length slice; position
        // of the mismatch never short-circuits.
        assert_eq!(
            constant_time_eq(b"\x00xxxxxxxx", b"\xffxxxxxxxx"),
            constant_time_eq(b"xxxxxxxx\x00", b"xxxxxxxx\xff"),
            "mismatch position must not change the result path"
        );
    }

    fn cfg_bind_token(bind: Option<&str>, token: Option<&str>) -> FleetConfig {
        let mut c = FleetConfig::defaults();
        c.bind = bind.map(|b| b.parse().expect("test bind addr"));
        c.auth_token = token.map(str::to_string);
        c
    }

    #[test]
    fn authorizer_for_loopback_no_token_is_allowall_open_posture() {
        // #10 posture preserved: loopback bind, no token ⇒ AllowAll
        // (open, localhost-only — D-FLEET §10.4).
        let c = cfg_bind_token(Some("127.0.0.1:8080"), None);
        let a = authorizer_for(&c).expect("loopback no-token must not error");
        assert!(a.authorize(None), "AllowAll ⇒ no-token allowed on loopback");
        assert!(a.authorize(Some("whatever")));
    }

    #[test]
    fn authorizer_for_non_loopback_no_token_fails_closed() {
        // THE load-bearing security property: a network-reachable bind
        // with no auth_token is REFUSED here (security_check by
        // construction) — never silently downgraded to AllowAll on a
        // public socket.
        let c = cfg_bind_token(Some("0.0.0.0:8080"), None);
        let r = authorizer_for(&c);
        assert!(
            matches!(r, Err(FleetConfigError::BadBind { .. })),
            "non-loopback + no token MUST be a refused config error \
             (Ok would mean a public socket got a silent AllowAll)"
        );
    }

    #[test]
    fn authorizer_for_token_present_is_bearer_even_on_loopback() {
        // Opting into auth is always honoured (loopback too).
        let c = cfg_bind_token(Some("127.0.0.1:8080"), Some("tok-XYZ"));
        let a = authorizer_for(&c).expect("token present ⇒ ok");
        assert!(a.authorize(Some("tok-XYZ")), "correct token allowed");
        assert!(!a.authorize(Some("tok-xyz")), "wrong token denied");
        assert!(!a.authorize(None), "no token denied when policy is bearer");
    }

    #[test]
    fn authorizer_for_non_loopback_with_token_is_bearer_enforced() {
        let c = cfg_bind_token(Some("0.0.0.0:8080"), Some("net-secret"));
        let a = authorizer_for(&c).expect("non-loopback + token ⇒ ok");
        assert!(a.authorize(Some("net-secret")));
        assert!(!a.authorize(Some("net-secre")));
        assert!(
            !a.authorize(None),
            "public bind w/ bearer ⇒ no-token denied"
        );
    }

    // ───────── CWDL #197: blank secret is not auth ─────────

    #[test]
    fn authorizer_for_non_loopback_blank_token_fails_closed() {
        // A blank (empty / whitespace-only) auth_token on a public bind
        // is treated as NO token ⇒ security_check refusal — never a
        // silent AllowAll or an empty-bearer on an off-host socket.
        for blank in ["", "   ", "\t "] {
            let c = cfg_bind_token(Some("0.0.0.0:8080"), Some(blank));
            let r = authorizer_for(&c);
            assert!(
                matches!(r, Err(FleetConfigError::BadBind { .. })),
                "non-loopback + blank {blank:?} MUST refuse (got Ok ⇒ \
                 unauthenticated public socket)"
            );
        }
    }

    #[test]
    fn bearer_with_empty_or_blank_secret_authorizes_nothing() {
        // Consumer-side make-the-bad-state-unrepresentable: even if an
        // empty/blank-secret BearerToken were constructed by some path,
        // it denies EVERY request (fail-closed → 401), never an
        // empty-`Bearer ` bypass.
        for blank in ["", "   ", "\t"] {
            let bt = BearerToken::new(blank);
            assert!(!bt.authorize(None), "blank-secret bearer denies None");
            assert!(
                !bt.authorize(Some("")),
                "blank-secret bearer denies empty presented"
            );
            assert!(
                !bt.authorize(Some(blank)),
                "blank-secret bearer denies the blank itself"
            );
            assert!(
                !bt.authorize(Some("anything")),
                "blank-secret bearer denies any token"
            );
        }
        // A loopback bind with a blank token ⇒ no token ⇒ AllowAll
        // (unchanged #10 localhost posture; blank only ever downgrades
        // where security_check already permits open).
        let c = cfg_bind_token(Some("127.0.0.1:8080"), Some("  "));
        let a = authorizer_for(&c).expect("loopback blank ⇒ AllowAll, not Err");
        assert!(a.authorize(None), "loopback no-effective-token ⇒ AllowAll");
    }

    #[test]
    fn authorizer_for_no_bind_defaults_open_v0_compat() {
        // No daemon/network at all (v0 default) ⇒ AllowAll, no error.
        let c = FleetConfig::defaults();
        let a = authorizer_for(&c).expect("no bind ⇒ no auth required");
        assert!(a.authorize(None));
    }

    #[test]
    fn request_roundtrips_and_rejects_unknown_op() {
        for r in [
            Request::GetStatus("w1".into()),
            Request::GetVerdict("w2".into()),
            Request::GetDiagnostics("w3".into()),
            Request::ListWorktrees,
            Request::Subscribe,
        ] {
            assert_eq!(Request::from_json(&r.to_json()), Some(r.clone()), "{r:?}");
        }
        assert_eq!(Request::from_json(r#"{"op":"nope"}"#), None);
        assert_eq!(Request::from_json("not json"), None);
        assert_eq!(Request::from_json("{}"), None);
    }

    #[test]
    fn batch_check_request_roundtrips_with_members_and_options() {
        let mut req = BatchCheckRequest::new("batch-42", "origin/dev");
        req.members = vec![
            BatchMember {
                worktree: "/client/wt-a".into(),
                files: vec![("src/a.rs".into(), "pub fn a() {}".into())],
                changed_files: vec!["src/a.rs".into()],
            },
            BatchMember {
                worktree: "/client/wt-b".into(),
                files: vec![("src/b.rs".into(), "pub fn b() {}".into())],
                changed_files: vec!["src/b.rs".into()],
            },
        ];
        req.check_profile = Some(CheckProfile {
            subcommand: CargoSubcommand::Clippy,
            package: Some("cargoless".into()),
            target: None,
            features: vec!["daemon".into()],
            no_default_features: false,
            release: false,
            extra_args: vec!["--all-targets".into()],
        });
        req.options = PushOverlayOptions {
            repo_relative: true,
            analysis_root: Some("/workspace/repo".into()),
            base_sha: Some("abc123".into()),
            changed_files: None,
            gate: true,
        };
        req.corun = false;

        let request = Request::BatchCheck(req);
        assert_eq!(
            Request::from_json(&request.to_json()),
            Some(request.clone()),
            "batch_check request must roundtrip exactly"
        );
    }

    #[test]
    fn batch_report_roundtrips_diagnostics_and_provenance() {
        let report = BatchReport {
            batch_id: "batch-red".into(),
            verdict: BatchVerdict::Red,
            members: vec![
                BatchMemberResult {
                    worktree: "wt-green".into(),
                    verdict: BatchVerdict::Green,
                    provenance: BatchProvenance::SoloGreen,
                    diagnostics: Vec::new(),
                    duration_ms: 12,
                },
                BatchMemberResult {
                    worktree: "wt-red".into(),
                    verdict: BatchVerdict::Red,
                    provenance: BatchProvenance::SoloRed,
                    diagnostics: vec![Diagnostic {
                        file_path: PathBuf::from("src/lib.rs"),
                        line: 7,
                        col: 3,
                        severity: Severity::Error,
                        code: Some("E0382".into()),
                        message: "borrow of moved value".into(),
                        source: Some("rustc".into()),
                    }],
                    duration_ms: 34,
                },
            ],
            combined_checks: 1,
            solo_checks: 2,
            duration_ms: 99,
        };

        assert_eq!(
            batchreport_from_json(&batchreport_to_json(&report)),
            Some(report)
        );
    }

    #[test]
    fn status_roundtrips_including_empty_crates_honesty_case() {
        // The #9/#11 sidecar invariant carried into the wire: empty
        // `crates` (untrustworthy / single-crate) must roundtrip as
        // empty — never silently become a bogus all-green list — and
        // `verdict` stands alone.
        let s = WorktreeStatus {
            worktree: "tf-mv-flat".into(),
            verdict: "red".into(),
            daemon_build_id: "test-build".into(),
            crates: vec![],
            red_diagnostics: 3,
            verdict_failure_reason: None,
            heartbeat_age_secs: 2,
            published_at: 1234567890,
        };
        assert_eq!(status_from_json(&status_to_json(&s)), Some(s));

        let s2 = WorktreeStatus {
            worktree: "tf-mv-check".into(),
            verdict: "red".into(),
            daemon_build_id: "test-build".into(),
            crates: vec![
                CrateVerdict {
                    name: "isolation".into(),
                    verdict: "green".into(),
                },
                CrateVerdict {
                    name: "physics".into(),
                    verdict: "red".into(),
                },
            ],
            red_diagnostics: 1,
            verdict_failure_reason: None,
            heartbeat_age_secs: 0,
            published_at: 42,
        };
        assert_eq!(status_from_json(&status_to_json(&s2)), Some(s2));
    }

    #[test]
    fn status_from_json_is_best_effort_never_panics() {
        assert_eq!(status_from_json(""), None);
        assert_eq!(status_from_json("garbage"), None);
        assert_eq!(status_from_json("{}"), None); // no worktree ⇒ None
        // Missing scalars default, never panic.
        let s = status_from_json(r#"{"worktree":"w"}"#).unwrap();
        assert_eq!(s.verdict, "unknown");
        assert_eq!(s.daemon_build_id, "");
        assert_eq!(s.red_diagnostics, 0);
        assert!(s.crates.is_empty());
    }

    #[test]
    fn summaries_roundtrip_and_tolerate_malformed_elements() {
        let list = vec![
            WorktreeSummary {
                worktree: "a".into(),
                verdict: "green".into(),
                daemon_build_id: "test-build".into(),
                red_diagnostics: 0,
            },
            WorktreeSummary {
                worktree: "b".into(),
                verdict: "red".into(),
                daemon_build_id: "test-build".into(),
                red_diagnostics: 2,
            },
        ];
        assert_eq!(summaries_from_json(&summaries_to_json(&list)), list);
        // A malformed element (no worktree) is skipped, not fatal.
        assert_eq!(
            summaries_from_json(
                r#"[{"verdict":"green"},{"worktree":"ok","verdict":"red","red_diagnostics":1}]"#
            ),
            vec![WorktreeSummary {
                worktree: "ok".into(),
                verdict: "red".into(),
                daemon_build_id: "".into(),
                red_diagnostics: 1
            }]
        );
    }

    #[test]
    fn allow_all_authorizes_with_or_without_token() {
        // #10 posture: open. #14 replaces this; the seam (not the
        // policy) is what #10 ships.
        assert!(AllowAll.authorize(None));
        assert!(AllowAll.authorize(Some("anything")));
    }

    // ───────── Increment 2 — PushOverlay verb + ack codec ─────────

    #[test]
    fn push_overlay_request_roundtrips_incl_empty_and_multi_files() {
        for files in [
            vec![],
            vec![("src/lib.rs".to_string(), "fn main(){}".to_string())],
            vec![
                ("Cargo.toml".to_string(), "[package]".to_string()),
                ("src/a.rs".to_string(), "// a".to_string()),
                ("src/b.rs".to_string(), String::new()),
            ],
        ] {
            let r = Request::PushOverlay {
                worktree: "wt-x".into(),
                base_ref: "origin/main".into(),
                files,
                check_profile: Some(CheckProfile {
                    subcommand: CargoSubcommand::Check,
                    package: Some("triform-server".into()),
                    target: Some("wasm32-unknown-unknown".into()),
                    features: vec!["hydrate".into()],
                    no_default_features: true,
                    release: true,
                    extra_args: vec![],
                }),
            };
            assert_eq!(
                Request::from_json(&r.to_json()),
                Some(r.clone()),
                "exact roundtrip: {r:?}"
            );
        }
    }

    #[test]
    fn push_overlay_check_profile_defaults_check_and_roundtrips_clippy() {
        let legacy = Request::from_json(
            r#"{"op":"push_overlay","worktree":"wt","base_ref":"b","files":[],
                "check_profile":{"package":"triform-alchemy"}}"#,
        )
        .unwrap();
        assert_eq!(
            legacy,
            Request::PushOverlay {
                worktree: "wt".into(),
                base_ref: "b".into(),
                files: vec![],
                check_profile: Some(CheckProfile {
                    subcommand: CargoSubcommand::Check,
                    package: Some("triform-alchemy".into()),
                    target: None,
                    features: vec![],
                    no_default_features: false,
                    release: false,
                    extra_args: vec![],
                }),
            }
        );

        let clippy = Request::PushOverlay {
            worktree: "wt".into(),
            base_ref: "b".into(),
            files: vec![],
            check_profile: Some(CheckProfile {
                subcommand: CargoSubcommand::Clippy,
                package: Some("triform-alchemy".into()),
                target: None,
                features: vec![],
                no_default_features: false,
                release: false,
                extra_args: vec![
                    "--manifest-path".into(),
                    "crates/alchemy/Cargo.toml".into(),
                    "--tests".into(),
                ],
            }),
        };
        assert_eq!(Request::from_json(&clippy.to_json()), Some(clippy));
    }

    #[test]
    fn push_overlay_v2_roundtrips_central_daemon_options() {
        let req = Request::PushOverlayV2 {
            worktree: "/client/wt".into(),
            base_ref: "origin/dev".into(),
            files: vec![("src/lib.rs".into(), "pub fn x() {}".into())],
            check_profile: None,
            options: PushOverlayOptions {
                repo_relative: true,
                analysis_root: Some("/workspace/tf-multiverse".into()),
                base_sha: Some("abc123".into()),
                changed_files: Some(vec!["src/lib.rs".into()]),
            },
        };
        assert_eq!(Request::from_json(&req.to_json()), Some(req));
    }

    #[test]
    fn push_overlay_from_json_is_best_effort_never_panics() {
        // Missing `files` ⇒ empty vec (not a panic); a malformed element
        // (no `path`) is skipped; missing `base_ref` ⇒ empty string —
        // same posture as `crate_verdicts_from_json`.
        let no_files = Request::from_json(r#"{"op":"push_overlay","worktree":"w"}"#).unwrap();
        assert_eq!(
            no_files,
            Request::PushOverlay {
                worktree: "w".into(),
                base_ref: String::new(),
                files: vec![],
                check_profile: None,
            }
        );
        let bad_elem = Request::from_json(
            r#"{"op":"push_overlay","worktree":"w","base_ref":"b",
                "files":[{"no_path":"x"},{"path":"ok.rs","content":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(
            bad_elem,
            Request::PushOverlay {
                worktree: "w".into(),
                base_ref: "b".into(),
                files: vec![("ok.rs".into(), "c".into())], // bad element skipped
                check_profile: None,
            }
        );
        // `files` not an array ⇒ empty, no panic.
        let not_array =
            Request::from_json(r#"{"op":"push_overlay","worktree":"w","files":"nope"}"#).unwrap();
        assert!(matches!(not_array, Request::PushOverlay { files, .. } if files.is_empty()));
        // Unknown op still `None` (the frozen rule is unchanged).
        assert_eq!(Request::from_json(r#"{"op":"frobnicate"}"#), None);
    }

    #[test]
    fn pushoverlayack_roundtrips_and_is_best_effort() {
        let a = PushOverlayAck {
            worktree: "wt-y".into(),
            accepted: true,
            applied_files: 7,
        };
        assert_eq!(
            pushoverlayack_from_json(&pushoverlayack_to_json(&a)),
            Some(a)
        );
        // Best-effort: no worktree ⇒ None; missing scalars ⇒ false/0.
        assert_eq!(pushoverlayack_from_json("{}"), None);
        assert_eq!(pushoverlayack_from_json("garbage"), None);
        let partial = pushoverlayack_from_json(r#"{"worktree":"w"}"#).unwrap();
        assert!(!partial.accepted);
        assert_eq!(partial.applied_files, 0);
    }

    #[test]
    fn verdict_service_default_push_overlay_refuses_honestly() {
        // The §2.4 contained seam-touch: the trait default reports it
        // stored nothing (`accepted:false`) — a service that has not
        // opted into push-ingest never falsely claims acceptance.
        use super::inproc::testmock::MockService;
        let ack = MockService::new().push_overlay("w", "base", &[]);
        assert_eq!(
            ack,
            PushOverlayAck {
                worktree: "w".into(),
                accepted: false,
                applied_files: 0,
            }
        );
    }

    #[test]
    fn verdict_service_default_batch_check_is_indeterminate_not_green() {
        use super::inproc::testmock::MockService;
        let mut request = BatchCheckRequest::new("batch-default", "origin/main");
        request.members = vec![BatchMember::new("wt-a")];

        let report = MockService::new().batch_check(&request);

        assert_eq!(report.verdict, BatchVerdict::Indeterminate);
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.members[0].worktree, "wt-a");
        assert_eq!(report.members[0].provenance, BatchProvenance::Indeterminate);
        assert!(
            report.members[0].diagnostics[0]
                .message
                .contains("unsupported")
        );
    }
}
