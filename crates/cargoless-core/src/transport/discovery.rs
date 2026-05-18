//! CLI auto-discovery fallback chain (`D-FLEET-SHARED-DAEMON` §10.3).
//!
//! `cargoless status` and friends resolve a transport in this strict
//! precedence:
//!
//! 1. `--remote <url>` given → **HTTP** (explicit operator intent wins).
//! 2. else a Unix socket at the conventional path has a **live
//!    listener** → **Unix socket** (the local-default fleet daemon is
//!    up). #185: liveness-probed, not bare-existence — a SIGKILL'd
//!    daemon's stale socket inode is treated as absent so step 3/4 take
//!    over (never a hard connect-refused; the #128/#129 class).
//! 3. else the on-disk `cli-status` file exists → **file-read** (the v0
//!    no-daemon behaviour — `cargoless watch` wrote a status file but
//!    there is no socket; works without any daemon process to talk to).
//! 4. else → **spawn a local single-binary daemon** (nothing is
//!    listening and nothing has run; bring one up in-process).
//!
//! The precedence *decision* is a pure function ([`resolve`]) — no
//! filesystem, no clock — so every branch is unit-tested deterministically.
//! [`discover`] is the thin I/O wrapper that probes socket/file existence
//! and applies [`resolve`]. Operator-friendly defaults (the CLI "just
//! works" with no flags) + deployment flexibility (network-distribute via
//! `--remote`) from one decision table.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The resolved transport the CLI should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// `--remote <url>` → HTTP adapter against this URL.
    Remote(String),
    /// Conventional Unix socket present → Unix adapter at this path.
    UnixSocket(PathBuf),
    /// No socket but a `cli-status` file → read it directly (v0
    /// no-daemon behaviour; the §10.3 step-3 fallback).
    FileRead(PathBuf),
    /// Nothing listening, nothing has run → spawn a local single-binary
    /// daemon (in-process).
    SpawnLocal,
}

/// The pure precedence decision (§10.3). Inputs are *facts already
/// probed* by [`discover`]; this function performs no I/O so the
/// four-way precedence is exhaustively unit-tested without a filesystem
/// or a live socket.
///
/// * `remote` — `Some(url)` iff the operator passed `--remote <url>`.
/// * `socket` — `Some(path)` iff a Unix socket exists at the
///   conventional path (already stat-probed).
/// * `status_file` — `Some(path)` iff the on-disk `cli-status` exists.
pub fn resolve(
    remote: Option<&str>,
    socket: Option<&Path>,
    status_file: Option<&Path>,
) -> Resolution {
    if let Some(url) = remote {
        return Resolution::Remote(url.to_string());
    }
    if let Some(sock) = socket {
        return Resolution::UnixSocket(sock.to_path_buf());
    }
    if let Some(sf) = status_file {
        return Resolution::FileRead(sf.to_path_buf());
    }
    Resolution::SpawnLocal
}

/// The conventional Unix-socket path for a repo root:
/// `<tmp>/cargoless-<hash>.sock`, where `<hash>` is a stable hash of the
/// **canonicalised** repo path (so two CLIs targeting the same repo —
/// possibly via different relative paths — agree on the socket, and two
/// different repos never collide). Uses `std::hash` `DefaultHasher`
/// (stable within a process run; the path is derived consistently by
/// every caller in that run, which is all the rendezvous needs — both
/// `serve --repo` and `cargoless status` compute it the same way). Pure.
pub fn conventional_socket_path(repo_root: &Path) -> PathBuf {
    // Canonicalise best-effort; fall back to the literal path if the
    // repo dir does not exist yet (the hash just needs to be consistent
    // for a given input, not canonical).
    let key = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    let tag = format!("{:016x}", h.finish());
    std::env::temp_dir().join(format!("cargoless-{tag}.sock"))
}

/// #185: confirm a Unix socket has a **live listener**, not merely that
/// the inode exists. A SIGKILL'd `serve --repo` leaves its socket file
/// behind (the same stale-daemon class as #128/#129, transport flavour);
/// a bare `Path::exists()` would then resolve [`Resolution::UnixSocket`]
/// and the CLI would hit a hard connect-refused instead of gracefully
/// falling through to the §10.3 FileRead tier.
///
/// The probe is `connect`, not `stat`: a *bound-but-busy* listener (a
/// slow `serve --repo` not yet `accept`ing) still completes `connect`
/// (the kernel queues it), so a live-but-loaded daemon is never
/// false-flagged dead. Only a stale inode with no listener returns
/// `ECONNREFUSED` (or `ENOENT` if the path vanished mid-probe).
/// Conservative #128/#129 posture: **only `Ok` counts as live** — any
/// error (refused / absent / permission / anything) ⇒ "not live" ⇒ fall
/// through safely; never fabricate a socket resolution. The connection
/// is dropped immediately (liveness is the only question asked).
#[cfg(unix)]
fn socket_is_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Non-unix: Unix sockets are unsupported anyway (cf. `transport::unix`
/// stubs), so a socket is never "live" here and discovery always falls
/// through to FileRead / spawn-local — identical to the socket being
/// absent. Keeps the un-cfg'd surface honest on every target.
#[cfg(not(unix))]
fn socket_is_live(_path: &Path) -> bool {
    false
}

/// I/O wrapper: probe the conventional socket **for a live listener**
/// (#185 — liveness, not bare existence) + the `cli-status` file, then
/// apply the pure [`resolve`]. `--remote` short-circuits before any
/// probe (explicit intent never pays a syscall).
///
/// `status_file` is the caller-supplied path to the v0 `cli-status`
/// (cli-crate-owned format; the cli passes its own resolved path — this
/// core fn only checks existence, it does not parse the cli format,
/// preserving the layering).
pub fn discover(remote: Option<&str>, repo_root: &Path, status_file: &Path) -> Resolution {
    if let Some(url) = remote {
        return Resolution::Remote(url.to_string());
    }
    let sock = conventional_socket_path(repo_root);
    // #185: liveness, not `sock.exists()`. A stale SIGKILL'd-daemon inode
    // is treated as absent so we fall through to FileRead, exactly as if
    // no daemon were there — never a hard connect-refused the caller has
    // to interpret.
    let sock_opt = if socket_is_live(&sock) {
        Some(sock.clone())
    } else {
        None
    };
    let sf_opt = if status_file.exists() {
        Some(status_file.to_path_buf())
    } else {
        None
    };
    resolve(remote, sock_opt.as_deref(), sf_opt.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_remote_beats_everything() {
        // --remote wins even when a socket AND a status file exist.
        assert_eq!(
            resolve(
                Some("http://host:8080"),
                Some(Path::new("/tmp/x.sock")),
                Some(Path::new("/p/.cargoless/cli-status")),
            ),
            Resolution::Remote("http://host:8080".into())
        );
    }

    #[test]
    fn precedence_socket_beats_file_and_spawn() {
        assert_eq!(
            resolve(
                None,
                Some(Path::new("/tmp/x.sock")),
                Some(Path::new("/p/.cargoless/cli-status")),
            ),
            Resolution::UnixSocket(PathBuf::from("/tmp/x.sock"))
        );
    }

    #[test]
    fn precedence_file_when_no_socket_is_the_v0_fallback() {
        // No socket, but `watch` wrote a status file ⇒ read it directly
        // (the v0 no-daemon behaviour — §10.3 step 3).
        assert_eq!(
            resolve(None, None, Some(Path::new("/p/.cargoless/cli-status"))),
            Resolution::FileRead(PathBuf::from("/p/.cargoless/cli-status"))
        );
    }

    #[test]
    fn precedence_spawn_local_when_nothing_present() {
        assert_eq!(resolve(None, None, None), Resolution::SpawnLocal);
    }

    #[test]
    fn conventional_socket_is_stable_and_repo_distinct() {
        let a1 = conventional_socket_path(Path::new("/repos/alpha"));
        let a2 = conventional_socket_path(Path::new("/repos/alpha"));
        let b = conventional_socket_path(Path::new("/repos/beta"));
        // Same repo ⇒ same socket (rendezvous); different repo ⇒
        // different socket (no cross-repo collision).
        assert_eq!(a1, a2, "same repo path must yield the same socket");
        assert_ne!(a1, b, "different repos must not collide");
        let name = a1.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("cargoless-") && name.ends_with(".sock"),
            "conventional name shape: {name}"
        );
    }

    #[test]
    fn discover_remote_short_circuits_without_probing() {
        // A non-existent repo path + non-existent status file, but
        // --remote given ⇒ Remote, no panic, no stat dependence.
        assert_eq!(
            discover(
                Some("http://h:1"),
                Path::new("/nonexistent/repo"),
                Path::new("/nonexistent/.cargoless/cli-status"),
            ),
            Resolution::Remote("http://h:1".into())
        );
    }

    #[test]
    fn discover_spawn_local_when_clean_slate() {
        // Real temp dir, no socket, no status file ⇒ SpawnLocal.
        let mut root = std::env::temp_dir();
        root.push(format!("cargoless-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sf = root.join(".cargoless").join("cli-status");
        assert_eq!(
            discover(None, &root, &sf),
            Resolution::SpawnLocal,
            "no socket + no status file ⇒ spawn local"
        );
        // Now create the status file ⇒ FileRead (the v0 fallback path).
        std::fs::create_dir_all(sf.parent().unwrap()).unwrap();
        std::fs::write(&sf, "schema=2\nverdict=green\n").unwrap();
        assert_eq!(discover(None, &root, &sf), Resolution::FileRead(sf.clone()));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- #185: stale-socket-inode liveness (the SIGKILL'd-daemon class)

    #[cfg(unix)]
    fn unique_repo(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cargoless-185-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn stale_socket_inode_falls_through_to_fileread_not_unix_socket() {
        // THE #185 bug: a SIGKILL'd `serve --repo` leaves its socket
        // inode (std `UnixListener` drop does NOT unlink the path — the
        // exact reason `transport::unix` has explicit remove_file
        // cleanup). Bind then drop ⇒ the file persists with NO listener
        // ⇒ connect ⇒ ECONNREFUSED. discover() MUST treat it as absent
        // and fall through to FileRead, not resolve a dead UnixSocket
        // that the CLI would hit a hard connect-refused on.
        let repo = unique_repo("stale");
        let sock = conventional_socket_path(&repo);
        let _ = std::fs::remove_file(&sock);
        {
            let _l = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
            // listener dropped at end of scope — inode remains, no owner
        }
        assert!(sock.exists(), "stale inode persists after listener drop");
        let sf = repo.join(".cargoless").join("cli-status");
        std::fs::create_dir_all(sf.parent().unwrap()).unwrap();
        std::fs::write(&sf, "schema=2\nverdict=green\n").unwrap();
        assert_eq!(
            discover(None, &repo, &sf),
            Resolution::FileRead(sf.clone()),
            "stale socket (exists but no listener) must fall through to FileRead, \
             never resolve a dead UnixSocket (#185 / #128-#129 class)"
        );
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn live_listener_resolves_unix_socket() {
        // The other side of the invariant: a genuinely-live listener
        // (connect succeeds) DOES resolve UnixSocket — the liveness
        // probe must not regress the happy path.
        let repo = unique_repo("live");
        let sock = conventional_socket_path(&repo);
        let _ = std::fs::remove_file(&sock);
        let _l = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        let sf = repo.join(".cargoless").join("cli-status");
        std::fs::create_dir_all(sf.parent().unwrap()).unwrap();
        std::fs::write(&sf, "schema=2\nverdict=green\n").unwrap();
        // Socket beats file (precedence) AND it is live ⇒ UnixSocket.
        assert_eq!(
            discover(None, &repo, &sf),
            Resolution::UnixSocket(sock.clone()),
            "a live listener must still resolve UnixSocket (no happy-path regression)"
        );
        drop(_l);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
