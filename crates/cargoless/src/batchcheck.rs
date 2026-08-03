//! `cargoless batch-check` — thin native batch gate client.
//!
//! This command is intentionally a wrapper around the transport request JSON
//! rather than a second CLI grammar for every batch member. Product wrappers
//! such as `dev-merge` already know their submitter overlays; they can write
//! one `{"op":"batch_check",...}` body. The command adds exact semantic
//! identities and emits a validated `cargoless.outcome/v3` envelope.

use std::path::PathBuf;
use std::process::ExitCode;

use cargoless_core::outcome::CheckState;
use cargoless_core::transport::Request;
use cargoless_core::transport::http::HttpClient;

/// EX_TEMPFAIL — the fleet-wide "escalate, do not fix" exit code: the gate
/// could not produce a real green/red verdict (indeterminate report, or a
/// transport/IO failure talking to the daemon). Distinct from 2 (caller
/// misuse: bad request file / malformed JSON / bad --remote).
const EXIT_TEMPFAIL: u8 = 75;

pub struct BatchCheckOpts {
    pub remote: String,
    pub auth_token: Option<String>,
    pub request_json: PathBuf,
}

/// The process convention is a projection of the serialized reaction, never a
/// second verdict interpretation.
fn reaction_exit_code(state: CheckState) -> u8 {
    match state {
        CheckState::Success | CheckState::NoUpdate => 0,
        CheckState::Failure => 1,
        CheckState::Pending | CheckState::Error => EXIT_TEMPFAIL,
    }
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
    let mut request = match Request::from_json(&body) {
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
    let semantic = match crate::verdict::attempt_context_from_env(&request.batch_id) {
        Ok(context) => context,
        Err(error) => {
            crate::ui::error(format!("batch-check: invalid semantic identity: {error}"));
            return ExitCode::from(2);
        }
    };
    request.options.semantic = Some(semantic);
    let body = Request::BatchCheck(request).to_json();
    let outcome = match client.submit_attempt_v3(&body) {
        Ok(outcome) => outcome,
        Err(e) => {
            crate::ui::error(format!("batch-check: remote `{}` failed: {e}", opts.remote));
            return ExitCode::from(EXIT_TEMPFAIL);
        }
    };

    println!(
        "{}",
        serde_json::to_string(&outcome).expect("outcome-v3 serializes")
    );
    ExitCode::from(reaction_exit_code(outcome.reaction.state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::batch::{BatchMember, BatchReport, BatchVerdict};
    use cargoless_core::transport::{
        BatchCheckRequest, batchreport_from_json, batchreport_to_json,
    };

    #[test]
    fn reaction_exit_codes_are_not_reinterpreted() {
        assert_eq!(reaction_exit_code(CheckState::Success), 0);
        assert_eq!(reaction_exit_code(CheckState::Failure), 1);
        assert_eq!(reaction_exit_code(CheckState::Error), 75);
        assert_eq!(reaction_exit_code(CheckState::Pending), 75);
        assert_eq!(reaction_exit_code(CheckState::NoUpdate), 0);
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
    fn unreadable_request_file_stays_caller_misuse_exit_2() {
        let code = run(&BatchCheckOpts {
            remote: "http://127.0.0.1:1".into(),
            auth_token: None,
            request_json: PathBuf::from("/cargoless-no-such-dir/request.json"),
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
    fn legacy_report_json_still_decodes_for_fixture_coverage() {
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
