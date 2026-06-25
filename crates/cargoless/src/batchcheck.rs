//! `cargoless batch-check` — thin native batch gate client.
//!
//! This command is intentionally a wrapper around the transport request JSON
//! rather than a second CLI grammar for every batch member. Product wrappers
//! such as `dev-merge` already know their submitter overlays; they can write
//! one `{"op":"batch_check",...}` body, send it through this command, and
//! consume the same JSON report the HTTP/Unix/in-proc adapters use.

use std::path::PathBuf;
use std::process::ExitCode;

use cargoless_core::batch::BatchVerdict;
use cargoless_core::transport::http::HttpClient;
use cargoless_core::transport::{Request, TransportClient, batchreport_to_json};

/// EX_TEMPFAIL — the fleet-wide "escalate, do not fix" exit code: the gate
/// could not produce a real green/red verdict (indeterminate report, or a
/// transport/IO failure talking to the daemon). Distinct from 2 (caller
/// misuse: bad request file / malformed JSON / bad --remote).
const EXIT_TEMPFAIL: u8 = 75;

pub struct BatchCheckOpts {
    pub remote: String,
    pub auth_token: Option<String>,
    pub request_json: PathBuf,
    /// `--advisory`: reify the operator-design advisory contract at the
    /// exit-code seam for a staged/pre-commit gate wrapping `batch-check`.
    /// Same shape as `verdict --advisory`: a real RED still hard-blocks
    /// (exit 1), but an `Indeterminate` report and a transport/IO failure
    /// (both "the gate could not produce a real verdict") exit 0 + a
    /// `[cargoless:advisory]` stderr line instead of 75 — the downstream
    /// compile-witness is the authoritative gate, so infra trouble must
    /// not drive a `--no-verify` bypass spiral. The report JSON is
    /// unchanged.
    pub advisory: bool,
}

/// Green ⇒ 0, Red ⇒ 1, Indeterminate ⇒ 75 ([`EXIT_TEMPFAIL`]).
///
/// Plain mode (no `--advisory`): pre-existing semantics — gate wrappers
/// already keyed off the 0/1/75 ladder are byte-identical.
fn verdict_exit_code(verdict: BatchVerdict) -> u8 {
    match verdict {
        BatchVerdict::Green => 0,
        BatchVerdict::Red => 1,
        BatchVerdict::Indeterminate => EXIT_TEMPFAIL,
    }
}

/// `--advisory`-aware exit mapping, mirroring `verdict::exit_byte_for_status`
/// at the batch seam:
///   * `Green` → 0
///   * `Red` → 1 (a real RED with member evidence still hard-blocks — the
///     one shape where a pre-commit/staged gate blocking is justified)
///   * `Indeterminate` → 0 + a `[cargoless:advisory]` stderr line (the gate
///     could not produce a trustworthy verdict; never hard-block on infra)
///
/// Plain mode falls through to [`verdict_exit_code`] so the legacy 0/1/75
/// ladder is byte-identical for non-advisory consumers.
fn advisory_exit_code(advisory: bool, verdict: BatchVerdict) -> u8 {
    if !advisory {
        return verdict_exit_code(verdict);
    }
    match verdict {
        BatchVerdict::Green => 0,
        BatchVerdict::Red => 1,
        BatchVerdict::Indeterminate => {
            log_advisory_skip(
                "indeterminate",
                "advisory: batch gate could not produce a trustworthy verdict — \
                 downstream witness is authoritative",
            );
            0
        }
    }
}

/// Structured stderr line for every `--advisory` skip — the same
/// `[cargoless:advisory]` prefix `verdict --advisory` emits, so an operator
/// can `grep -F '[cargoless:advisory]'` across both commands for every
/// degraded path a hook silently let through.
fn log_advisory_skip(verdict: &str, detail: &str) {
    eprintln!("[cargoless:advisory] verdict={verdict} class=- reason=- note={detail}");
}

pub fn run(opts: &BatchCheckOpts) -> ExitCode {
    let body = match std::fs::read_to_string(&opts.request_json) {
        Ok(body) => body,
        Err(e) => {
            crate::ui::error(format!(
                "batch-check: could not read `{}`: {e}",
                opts.request_json.display()
            ));
            return ExitCode::from(2);
        }
    };
    let request = match Request::from_json(&body) {
        Some(Request::BatchCheck(request)) => request,
        _ => {
            crate::ui::error(format!(
                "batch-check: `{}` is not a valid batch_check request JSON",
                opts.request_json.display()
            ));
            return ExitCode::from(2);
        }
    };
    let client = match opts.auth_token.as_deref() {
        Some(token) => HttpClient::with_token(&opts.remote, token),
        None => HttpClient::new(&opts.remote),
    };
    let client = match client {
        Ok(client) => client,
        Err(e) => {
            crate::ui::error(format!(
                "batch-check: HttpClient init failed for `{}`: {e}",
                opts.remote
            ));
            return ExitCode::from(2);
        }
    };
    let report = match client.batch_check(&request) {
        Ok(report) => report,
        Err(e) => {
            crate::ui::error(format!("batch-check: remote `{}` failed: {e}", opts.remote));
            // Transport/IO failure: no verdict was produced — EX_TEMPFAIL,
            // so gate wrappers escalate instead of treating it as red. Under
            // `--advisory` this is the same "infra could not evaluate" shape
            // as an Indeterminate report: exit 0 + the advisory stderr line,
            // never a hard-block (a daemon-down pre-commit must not push the
            // operator onto `--no-verify`).
            if opts.advisory {
                log_advisory_skip(
                    "unknown",
                    "advisory: batch transport failed (no verdict produced) — \
                     downstream witness is authoritative",
                );
                return ExitCode::from(0);
            }
            return ExitCode::from(EXIT_TEMPFAIL);
        }
    };

    // Machine-readable stdout. Stderr gets human errors/logging.
    println!("{}", batchreport_to_json(&report));
    ExitCode::from(advisory_exit_code(opts.advisory, report.verdict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::batch::{BatchMember, BatchReport};
    use cargoless_core::transport::{BatchCheckRequest, batchreport_from_json};

    #[test]
    fn verdict_exit_codes_green_0_red_1_indeterminate_75() {
        assert_eq!(verdict_exit_code(BatchVerdict::Green), 0);
        assert_eq!(verdict_exit_code(BatchVerdict::Red), 1);
        // 75 = EX_TEMPFAIL, the fleet-wide escalate-do-not-fix convention.
        assert_eq!(verdict_exit_code(BatchVerdict::Indeterminate), 75);
    }

    #[test]
    fn advisory_exit_code_downgrades_indeterminate_keeps_red() {
        // --advisory mirrors verdict --advisory at the batch seam: a real
        // RED still hard-blocks (1), Green is 0, and Indeterminate (infra
        // could not produce a trustworthy verdict) downgrades 75 → 0.
        assert_eq!(advisory_exit_code(true, BatchVerdict::Green), 0);
        assert_eq!(
            advisory_exit_code(true, BatchVerdict::Red),
            1,
            "a real RED with member evidence still hard-blocks under --advisory"
        );
        assert_eq!(
            advisory_exit_code(true, BatchVerdict::Indeterminate),
            0,
            "Indeterminate is infra trouble — never a hard-block under --advisory"
        );
    }

    #[test]
    fn advisory_plain_mode_is_byte_identical_to_legacy_ladder() {
        // Non-advisory consumers (pollers, CI gates) keyed off the 0/1/75
        // ladder must be unaffected: advisory_exit_code(false, ..) ==
        // verdict_exit_code(..) for every verdict.
        for v in [
            BatchVerdict::Green,
            BatchVerdict::Red,
            BatchVerdict::Indeterminate,
        ] {
            assert_eq!(advisory_exit_code(false, v), verdict_exit_code(v), "{v:?}");
        }
    }

    #[test]
    fn transport_error_exits_75_not_red() {
        let dir = std::env::temp_dir().join(format!(
            "cargoless-batchcheck-tempfail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let request_json = dir.join("request.json");
        let mut request = BatchCheckRequest::new("batch-tempfail", "origin/main");
        request.members = vec![BatchMember::new("wt-a")];
        std::fs::write(&request_json, Request::BatchCheck(request).to_json()).unwrap();

        // Port 9 is intentionally not served (same convention as the
        // transport's own connect-refused tests): connect fails fast, no
        // daemon answers — the canonical "daemon down" transport error.
        let code = run(&BatchCheckOpts {
            remote: "http://127.0.0.1:9".into(),
            auth_token: None,
            request_json,
            advisory: false,
        });

        // ExitCode has no PartialEq; its Debug repr is stable within one
        // platform, so compare against the constructor we expect.
        assert_eq!(
            format!("{code:?}"),
            format!("{:?}", ExitCode::from(EXIT_TEMPFAIL))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn advisory_transport_error_exits_0_not_tempfail() {
        // --advisory: a daemon-down transport failure produced no verdict,
        // so it is infra trouble, not a code red. A staged/pre-commit gate
        // must NOT hard-block on it (that is the --no-verify spiral this
        // contract eliminates) — exit 0, the witness is authoritative.
        let dir = std::env::temp_dir().join(format!(
            "cargoless-batchcheck-advisory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let request_json = dir.join("request.json");
        let mut request = BatchCheckRequest::new("batch-advisory-tempfail", "origin/main");
        request.members = vec![BatchMember::new("wt-a")];
        std::fs::write(&request_json, Request::BatchCheck(request).to_json()).unwrap();

        let code = run(&BatchCheckOpts {
            remote: "http://127.0.0.1:9".into(), // unserved port: connect refused
            auth_token: None,
            request_json,
            advisory: true,
        });
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(0)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unreadable_request_file_stays_caller_misuse_exit_2() {
        let code = run(&BatchCheckOpts {
            remote: "http://127.0.0.1:1".into(),
            auth_token: None,
            request_json: PathBuf::from("/cargoless-no-such-dir/request.json"),
            advisory: false,
        });
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
    }

    #[test]
    fn request_json_uses_the_transport_shape() {
        let mut request = BatchCheckRequest::new("batch-cli", "origin/main");
        request.members = vec![BatchMember::new("wt-a")];
        let encoded = Request::BatchCheck(request.clone()).to_json();
        assert_eq!(
            Request::from_json(&encoded),
            Some(Request::BatchCheck(request))
        );
    }

    #[test]
    fn report_json_is_the_stdout_shape() {
        let report = BatchReport {
            batch_id: "batch-cli".into(),
            verdict: BatchVerdict::Green,
            members: Vec::new(),
            combined_checks: 1,
            solo_checks: 0,
            duration_ms: 42,
            queue_wait_ms: 0,
            executed_members: 0,
            executed_batch_id: Some("batch-cli".into()),
        };
        assert_eq!(
            batchreport_from_json(&batchreport_to_json(&report)),
            Some(report)
        );
    }
}
