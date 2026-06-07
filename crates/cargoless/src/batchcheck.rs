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

pub struct BatchCheckOpts {
    pub remote: String,
    pub auth_token: Option<String>,
    pub request_json: PathBuf,
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
            return ExitCode::from(2);
        }
    };

    // Machine-readable stdout. Stderr gets human errors/logging.
    println!("{}", batchreport_to_json(&report));
    match report.verdict {
        BatchVerdict::Green => ExitCode::SUCCESS,
        BatchVerdict::Red => ExitCode::from(1),
        BatchVerdict::Indeterminate => ExitCode::from(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_core::batch::{BatchMember, BatchReport};
    use cargoless_core::transport::{BatchCheckRequest, batchreport_from_json};

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
