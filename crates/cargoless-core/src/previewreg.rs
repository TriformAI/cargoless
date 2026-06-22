//! `previewreg` — the durable registry of runtime self-serve previews.
//!
//! Static instances come from the `--instances` file and are re-read at boot.
//! Runtime previews (registered via `POST /instances`) have no such file, so
//! without this module they live only in the daemon's in-memory `live_specs`
//! and vanish on any pod restart (image bump, OOM, liveness self-heal, drain) —
//! the agent's `<name>.tryform.wtf` would silently disappear and the reconciler
//! would prune its route. This registry is the previews' equivalent of the
//! instances file: the daemon rewrites it on every add/remove and re-reads it at
//! boot to re-register each preview (re-bind its proxy port, respawn its
//! last-green via the normal `appstatefile` recovery, restart its ref poller).
//!
//! One file for the whole preview set (not one-per-preview like `appstatefile`)
//! because the set is read+rewritten atomically as a unit on each mutation —
//! the same temp+fsync+rename primitive (`write_pointer_atomic`), so a crash
//! mid-write leaves the previous registry byte-intact.
//!
//! Flat, scheme-headed, no serde — the same byte-discipline as `appstatefile`
//! and the latest-green pointer. One record per line; fields are `|`-separated
//! (a char that cannot appear in a sanitized preview name, a git ref, a
//! host:port, or our k=v env joined with `;`). Env values have `|`/`;`/newlines
//! stripped on write (non-secret overlay values are tame); the daemon re-derives
//! secret/base env from its own process env + `--preview-defaults` at respawn, so
//! the registry only needs the per-preview *overlay*, not the full child env.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::build::write_pointer_atomic;

/// Scheme header — bump only on an incompatible format change (an unknown
/// scheme is treated as an absent/empty registry rather than misparsed).
const SCHEME: &str = "cargoless-preview-registry/1";

/// One persisted runtime preview. Mirrors the live `InstanceSpec` plus the
/// lifetime/idle bookkeeping the in-memory control loop keeps in side-maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRecord {
    pub name: String,
    pub git_ref: String,
    pub app_bind: SocketAddr,
    /// Unix-seconds expiry instant (TTL). 0 ⇒ no expiry recorded.
    pub expires_at: u64,
    /// Last time a request was proxied to this preview (unix seconds), for
    /// idle-eviction. 0 ⇒ never observed (treated as "active since boot").
    pub last_active_unix: u64,
    /// Whether an isolated per-branch DB was requested (advisory; replayed so
    /// the respawn logs the same intent).
    pub own_db: bool,
    /// The per-preview env *overlay* (the client's `--env` plus the host-URL
    /// overrides). NOT the full child env — base/secret env is re-derived from
    /// the daemon process env + `--preview-defaults` at respawn.
    pub env: BTreeMap<String, String>,
}

/// `<state_dir>/app/previews.registry`.
pub fn registry_path(state_dir: &Path) -> PathBuf {
    state_dir.join("app").join("previews.registry")
}

/// Render the full preview set to the durable flat format.
pub fn render(previews: &[PreviewRecord]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(SCHEME);
    s.push('\n');
    for p in previews {
        // env as k=v pairs joined by ';', each component sanitized of the
        // framing chars so a value can never break the line/field grammar.
        let env_joined = p
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", sanitize(k), sanitize(v)))
            .collect::<Vec<_>>()
            .join(";");
        let _ = writeln!(
            s,
            "{}|{}|{}|{}|{}|{}|{}",
            sanitize(&p.name),
            sanitize(&p.git_ref),
            p.app_bind,
            p.expires_at,
            p.last_active_unix,
            u8::from(p.own_db),
            env_joined,
        );
    }
    s
}

/// Atomically write the preview set. A crash mid-write leaves the prior
/// registry byte-intact (temp+fsync+rename).
pub fn write(state_dir: &Path, previews: &[PreviewRecord]) -> std::io::Result<()> {
    write_pointer_atomic(&registry_path(state_dir), &render(previews))
}

/// Read the persisted preview set. `None` ⇒ absent file or unknown scheme
/// (treat as "no previews"). A known scheme with some malformed lines parses
/// the good lines and skips the bad ones — boot recovery must never wedge on a
/// corrupt registry; a dropped preview just re-registers on next use.
pub fn read(state_dir: &Path) -> Option<Vec<PreviewRecord>> {
    let text = std::fs::read_to_string(registry_path(state_dir)).ok()?;
    parse(&text)
}

/// Pure parse of the flat format. Separated from [`read`] for unit testing.
pub fn parse(text: &str) -> Option<Vec<PreviewRecord>> {
    let mut lines = text.lines();
    if lines.next() != Some(SCHEME) {
        return None; // absent scheme / wrong version ⇒ "no previews"
    }
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        // name|git_ref|app_bind|expires_at|last_active|own_db|env
        let parts: Vec<&str> = line.splitn(7, '|').collect();
        if parts.len() < 7 {
            continue; // malformed line — skip, don't wedge
        }
        let Ok(app_bind) = parts[2].parse::<SocketAddr>() else {
            continue;
        };
        let mut env = BTreeMap::new();
        if !parts[6].is_empty() {
            for kv in parts[6].split(';') {
                if let Some((k, v)) = kv.split_once('=') {
                    env.insert(k.to_string(), v.to_string());
                }
            }
        }
        out.push(PreviewRecord {
            name: parts[0].to_string(),
            git_ref: parts[1].to_string(),
            app_bind,
            expires_at: parts[3].parse().unwrap_or(0),
            last_active_unix: parts[4].parse().unwrap_or(0),
            own_db: parts[5] == "1",
            env,
        });
    }
    Some(out)
}

/// Strip the framing characters (`|`, `;`, newlines) from a value so it can
/// never break the line/field grammar. Preview names are already DNS-safe and
/// git refs/host:ports never contain these; this only ever bites a pathological
/// `--env` value, which loses the stripped chars (acceptable for a non-secret
/// overlay) rather than corrupting the registry.
fn sanitize(s: &str) -> String {
    s.replace(['|', ';', '\n', '\r'], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, port: u16, expires: u64) -> PreviewRecord {
        let mut env = BTreeMap::new();
        env.insert(
            "TRIFORM_PUBLIC_BASE_URL".into(),
            format!("https://{name}.tryform.wtf"),
        );
        PreviewRecord {
            name: name.into(),
            git_ref: format!("origin/{name}"),
            app_bind: format!("0.0.0.0:{port}").parse().unwrap(),
            expires_at: expires,
            last_active_unix: 0,
            own_db: false,
            env,
        }
    }

    #[test]
    fn round_trips_a_preview_set() {
        let set = vec![rec("feat-a", 8200, 1_700_000_000), rec("feat-b", 8201, 0)];
        let text = render(&set);
        let parsed = parse(&text).expect("known scheme parses");
        assert_eq!(
            parsed, set,
            "the set round-trips byte-for-byte through render/parse"
        );
    }

    #[test]
    fn unknown_scheme_is_no_previews() {
        assert_eq!(parse("some-other/9\nfoo|bar\n"), None);
        assert_eq!(parse(""), None);
        // Known scheme, zero records ⇒ Some(empty) (a daemon with no previews).
        assert_eq!(parse(&format!("{SCHEME}\n")), Some(vec![]));
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let good = render(&[rec("ok", 8200, 5)]);
        // Inject a junk line; the good record must still parse.
        let mixed = good.replace(
            &format!("{SCHEME}\n"),
            &format!("{SCHEME}\ngarbage-line\n|||\n"),
        );
        let parsed = parse(&mixed).expect("scheme present");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "ok");
    }

    #[test]
    fn env_framing_chars_are_stripped() {
        let mut r = rec("x", 8200, 0);
        r.env.insert("K".into(), "a|b;c\nd".into());
        let text = render(&[r]);
        // The written line has exactly the 6 field separators + env, no extras.
        let line = text.lines().nth(1).unwrap();
        assert_eq!(
            line.matches('|').count(),
            6,
            "env value did not inject a `|`"
        );
        let back = parse(&text).unwrap();
        assert_eq!(back[0].env.get("K").map(String::as_str), Some("abcd"));
    }

    #[test]
    fn write_then_read_through_the_filesystem() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cargoless-previewreg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Absent ⇒ None.
        assert_eq!(read(&dir), None);
        let set = vec![rec("feat", 8200, 99)];
        write(&dir, &set).unwrap();
        assert_eq!(read(&dir).as_deref(), Some(set.as_slice()));
        // Rewrite replaces in full (atomic, never appends).
        write(&dir, &[]).unwrap();
        assert_eq!(read(&dir), Some(vec![]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
