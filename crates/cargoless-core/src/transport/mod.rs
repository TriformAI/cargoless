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

use std::sync::mpsc::Receiver;

use cargoless_proto::Diagnostic;

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

/// Transport-agnostic worktree status (the `get_status` payload, §10.1).
/// `crates` empty ⇒ no trustworthy per-crate breakdown (single-crate, or
/// the #9 unattributable-error honesty case); `verdict` is **always** the
/// authoritative tree verdict and stands alone — the sidecar discipline
/// (#11/#176) carried into the transport layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub worktree: String,
    pub verdict: String,
    pub crates: Vec<CrateVerdict>,
    pub red_diagnostics: u32,
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
    pub published_at: u64,
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
        };
        v.to_string()
    }
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
pub fn status_to_json(s: &WorktreeStatus) -> String {
    serde_json::json!({
        "worktree": s.worktree,
        "verdict": s.verdict,
        "crates": crate_verdicts_json(&s.crates),
        "red_diagnostics": s.red_diagnostics,
        "heartbeat_age_secs": s.heartbeat_age_secs,
        "published_at": s.published_at,
    })
    .to_string()
}

/// Parse wire JSON back to a `WorktreeStatus` (best-effort: a missing
/// `worktree` ⇒ `None`; missing scalars ⇒ 0/empty, never a panic).
pub fn status_from_json(text: &str) -> Option<WorktreeStatus> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(WorktreeStatus {
        worktree: v.get("worktree")?.as_str()?.to_string(),
        verdict: v
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        crates: crate_verdicts_from_json(v.get("crates")),
        red_diagnostics: v
            .get("red_diagnostics")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
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
                red_diagnostics: s
                    .get("red_diagnostics")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
            })
        })
        .collect()
}

/// Serialise a transition event (SSE `data:` payload / Unix NDJSON line).
pub fn event_to_json(e: &TransitionEvent) -> String {
    serde_json::json!({
        "worktree": e.worktree,
        "verdict": e.verdict,
        "red_diagnostics": e.red_diagnostics,
        "published_at": e.published_at,
    })
    .to_string()
}

/// Parse a transition event from its wire JSON (the `subscribe` NDJSON
/// frame / SSE `data:` payload). Shared by the Unix + HTTP stream
/// clients so both decode byte-identically. Best-effort: a malformed
/// frame ⇒ `None` (the stream client skips it, never panics).
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
        published_at: v
            .get("published_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn status_roundtrips_including_empty_crates_honesty_case() {
        // The #9/#11 sidecar invariant carried into the wire: empty
        // `crates` (untrustworthy / single-crate) must roundtrip as
        // empty — never silently become a bogus all-green list — and
        // `verdict` stands alone.
        let s = WorktreeStatus {
            worktree: "tf-mv-flat".into(),
            verdict: "red".into(),
            crates: vec![],
            red_diagnostics: 3,
            heartbeat_age_secs: 2,
            published_at: 1234567890,
        };
        assert_eq!(status_from_json(&status_to_json(&s)), Some(s));

        let s2 = WorktreeStatus {
            worktree: "tf-mv-check".into(),
            verdict: "red".into(),
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
        assert_eq!(s.red_diagnostics, 0);
        assert!(s.crates.is_empty());
    }

    #[test]
    fn summaries_roundtrip_and_tolerate_malformed_elements() {
        let list = vec![
            WorktreeSummary {
                worktree: "a".into(),
                verdict: "green".into(),
                red_diagnostics: 0,
            },
            WorktreeSummary {
                worktree: "b".into(),
                verdict: "red".into(),
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
}
