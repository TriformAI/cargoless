//! Model R cache layout (#7): pinned `base.cache` + `tree.cache` + `solo` +
//! `combined`, with the operator's **decoupled-lifecycle write-gate**.
//!
//! ## What this module is
//!
//! Model R serves N worktrees off ONE rust-analyzer. The verdict-cache that
//! backs that has four regions with *deliberately different lifecycles*
//! (design `D-FLEET-SHARED-DAEMON.md §5`):
//!
//! ```text
//! <state-root>/                    ← e.g. <repo>/.triform/cargoless  (configurable)
//!   base.cache/                    ← pinned to last git pull/rebase;
//!                                     immutable until an EXPLICIT git-advance
//!   tree.cache/                    ← base worktree [dev]'s in-flight overlay
//!                                     (the operator's own un-committed edits)
//!   solo/<hash(HW)>                ← one worktree's solo verdict, content-addressed
//!   combined/<hash(sort{HW…})>     ← corun combined-overlay verdict, content-addressed
//!
//! <each-worktree>/<state-dir>/
//!   tree.cache/                    ← that worktree's overlay state + diagnostics
//! ```
//!
//! ## The invariant this module *enforces* (not merely documents)
//!
//! > **`base.cache` is write-gated to explicit git-advance ops ONLY.**
//! > In-flight edits in base `[dev]` flow to `tree.cache`, **never** to
//! > `base.cache`.
//!
//! This is the operator's decoupled-lifecycle insight and it is launch-critical
//! (the whole "rapid [dev] edits don't re-derive across 20 active worktrees"
//! property rests on it). It is enforced *structurally*, not by convention:
//!
//! * the **only** API that mutates anything under `base.cache/` is
//!   [`CacheLayout::advance_base`], whose name and doc make the git-advance
//!   precondition unmissable;
//! * every in-flight-edit path ([`CacheLayout::tree_cache_dir`],
//!   [`CacheLayout::solo_entry`], [`CacheLayout::combined_entry`],
//!   [`CacheLayout::worktree_tree_cache`]) resolves to a *different directory*
//!   and shares no mutable handle with `base.cache/` — there is no code path
//!   from an edit to `base.cache`;
//! * the regression guard [`tests::base_cache_is_byte_unmoved_by_inflight_writes`]
//!   asserts `base.cache/` is byte-identical after a storm of tree/solo/combined
//!   writes — the same "byte-unmoved" discipline AC#4's never-publish-red uses.
//!
//! ## Keyspace safety (mirrors `cargoless-cas::identity`)
//!
//! A combined-corun entry is keyed by a *set* of overlay hashes. A naive
//! `hash(a + b)` would let `{ "ab" }` alias `{ "a", "b" }` and `{A,B}` alias
//! `{B,A}` — and a cache-key collision here is not a hash weakness, it is a
//! *wrong-verdict* bug (serve worktree X's verdict for set Y). [`combined_key`]
//! therefore reuses the exact canonical-encoding discipline `identity.rs` uses
//! for `InputHash`: sort + dedup the set, then a scheme-versioned,
//! length-prefixed, tagged preimage. Its scheme tag is distinct from every CAS
//! keyspace so a corun key can never alias a `BuildIdentity` `InputHash`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cargoless_cas::sha256_hex;

/// Backward-compat / OSS-out-of-the-box state dir, relative to a repo/worktree
/// root. v0 single-worktree `cargoless watch` uses this; nothing changes for
/// existing users.
pub const DEFAULT_STATE_DIR_REL: &str = ".cargoless";

/// The operator's tf-multiverse convention (matches their existing
/// `.triform/` organizational layout). Selected via the config layer
/// (`--state-dir` / `TF_STATE_DIR` / `tf.toml [project] state_dir`); this
/// module only consumes the *resolved* path — it does not re-derive config.
pub const TF_STATE_DIR_REL: &str = ".triform/cargoless";

const BASE_CACHE: &str = "base.cache";
const TREE_CACHE: &str = "tree.cache";
const SOLO: &str = "solo";
const COMBINED: &str = "combined";

/// The pin marker filename written inside `base.cache/`. Records the
/// [`BasePin`] the cache region is currently pinned to. Its presence is also
/// the "base.cache has been advanced at least once" signal.
const PIN_MARKER: &str = "PIN";

/// Scheme version for the combined-corun key preimage. Bumping this is a
/// deliberate, repo-visible move of the entire corun keyspace (every prior
/// `combined/<key>` becomes unreachable) — never silent, exactly like
/// `cargoless-cas::identity`'s frozen `input_hash` SCHEME constant. (That
/// constant's own wire string is a §9a-frozen pre-rename literal, kept
/// verbatim + allowlisted *there* because renaming it would move the whole
/// CAS keyspace; this brand-new corun keyspace deliberately uses a
/// cargoless-branded scheme tag instead — no old-brand residual.) The
/// distinct path segment (`combined-overlay-set`) guarantees a corun key
/// can never collide with a build-identity `InputHash` even at the same
/// SHA-256 primitive.
const COMBINED_SCHEME: &[u8] = b"cargoless/cache/combined-overlay-set/v1";

/// Content hash of one worktree's overlay-set (the `git diff base..<wt>` file
/// set). This module does **not** compute it — that is #5 (LSP overlay
/// multiplexing) / #8 (corun). Here it is purely the cache-key newtype, kept
/// distinct from `cargoless-proto`'s `ContentHash`/`InputHash` so the corun
/// keyspace stays its own audited space.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OverlayHash(String);

impl OverlayHash {
    pub fn new(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The git identity `base.cache` is pinned to — the commit-ish the base
/// worktree `[dev]` was at when an explicit git-advance op last advanced the
/// pinned region. Recorded so a re-attaching daemon can tell whether
/// `base.cache` is still valid for the current base checkout without trusting
/// directory mtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasePin(String);

impl BasePin {
    /// `rev` is whatever uniquely identifies the advanced base state — in
    /// practice `git rev-parse HEAD` of the base worktree at the git-advance
    /// op. Kept opaque on purpose: #7 does not run git; the caller (the
    /// repo-scoped daemon, #3) supplies the resolved revision.
    pub fn new(rev: impl Into<String>) -> Self {
        Self(rev.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The four-region cache layout rooted at an already-resolved state root.
///
/// Construct with [`CacheLayout::at`] when the config layer (#1) has resolved
/// the absolute state path, or [`CacheLayout::for_repo`] for the common case of
/// `<repo-root>/<state-dir-rel>`.
#[derive(Debug, Clone)]
pub struct CacheLayout {
    /// Absolute (or repo-relative) state root, e.g.
    /// `<repo>/.triform/cargoless`.
    root: PathBuf,
    /// The state-dir component relative to *any* worktree root (e.g.
    /// `.triform/cargoless`). Used to locate per-worktree `tree.cache/`.
    state_dir_rel: PathBuf,
}

impl CacheLayout {
    /// Use when the config layer has already resolved the absolute state root
    /// AND you separately know the per-worktree state-dir component (needed to
    /// locate `<wt>/<state-dir>/tree.cache`). Most callers want
    /// [`CacheLayout::for_repo`] instead.
    pub fn at(state_root: impl Into<PathBuf>, state_dir_rel: impl Into<PathBuf>) -> Self {
        Self {
            root: state_root.into(),
            state_dir_rel: state_dir_rel.into(),
        }
    }

    /// The common case: state root is `<repo_root>/<state_dir_rel>`. With
    /// `state_dir_rel = DEFAULT_STATE_DIR_REL` this is the v0 backward-compat
    /// layout; with `TF_STATE_DIR_REL` it is the operator's tf-multiverse
    /// layout.
    pub fn for_repo(repo_root: impl AsRef<Path>, state_dir_rel: impl Into<PathBuf>) -> Self {
        let state_dir_rel = state_dir_rel.into();
        let root = repo_root.as_ref().join(&state_dir_rel);
        Self {
            root,
            state_dir_rel,
        }
    }

    /// The resolved state root (`<repo>/<state-dir>`).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<state-root>/base.cache` — the pinned region. **Never written except
    /// through [`advance_base`](CacheLayout::advance_base).** Reading it (e.g.
    /// a cache lookup keyed against the pinned base) is always fine.
    #[must_use]
    pub fn base_cache_dir(&self) -> PathBuf {
        self.root.join(BASE_CACHE)
    }

    /// `<state-root>/tree.cache` — the base worktree `[dev]`'s in-flight
    /// overlay. **This is where the operator's own un-committed edits go.**
    /// Mutating this never touches `base.cache` (different directory).
    #[must_use]
    pub fn tree_cache_dir(&self) -> PathBuf {
        self.root.join(TREE_CACHE)
    }

    /// `<state-root>/solo` — the per-worktree solo verdict cache directory.
    #[must_use]
    pub fn solo_dir(&self) -> PathBuf {
        self.root.join(SOLO)
    }

    /// `<state-root>/combined` — the corun combined-overlay verdict cache
    /// directory.
    #[must_use]
    pub fn combined_dir(&self) -> PathBuf {
        self.root.join(COMBINED)
    }

    /// `<state-root>/solo/<HW>` — content-addressed: a given overlay-hash
    /// always maps to the same entry, and a *different* overlay produces a
    /// *different* entry (old one retained, CAS immutability). The base
    /// advancing does not invalidate this; it just shifts the overlay
    /// reference.
    #[must_use]
    pub fn solo_entry(&self, hw: &OverlayHash) -> PathBuf {
        self.solo_dir().join(hw.as_str())
    }

    /// `<state-root>/combined/<H(sort{HW…})>` — the corun combined entry for a
    /// *set* of worktree overlays. Set-keyed: order and duplicates do not
    /// matter (see [`combined_key`]). Additive to `solo/` — content-addressing
    /// makes this an extra layer, not a replacement (design §7.2).
    #[must_use]
    pub fn combined_entry(&self, set: &[OverlayHash]) -> PathBuf {
        self.combined_dir().join(combined_key(set))
    }

    /// `<wt_root>/<state-dir>/tree.cache` — a *non-base* worktree's overlay
    /// state + diagnostics. This lives under the worktree itself, NOT under
    /// the base state root, so each worktree's edits are isolated and base
    /// advances cannot touch a worktree's cache (design §5 lifecycle table).
    #[must_use]
    pub fn worktree_tree_cache(&self, wt_root: impl AsRef<Path>) -> PathBuf {
        wt_root.as_ref().join(&self.state_dir_rel).join(TREE_CACHE)
    }

    /// Create the base state-root regions (`base.cache/`, `tree.cache/`,
    /// `solo/`, `combined/`). Idempotent. Does NOT create per-worktree
    /// `tree.cache/` dirs — those are created lazily on first activity by the
    /// activity-activation layer (#12).
    ///
    /// # Errors
    /// Propagates the underlying [`io::Error`] if a directory cannot be
    /// created.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for d in [
            self.base_cache_dir(),
            self.tree_cache_dir(),
            self.solo_dir(),
            self.combined_dir(),
        ] {
            fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// Read the current [`BasePin`] `base.cache/` is pinned to, or `None` if
    /// it has never been advanced (fresh layout). Cheap, side-effect-free.
    ///
    /// # Errors
    /// Propagates an [`io::Error`] only for *unexpected* failures (e.g.
    /// permission denied); a missing marker is `Ok(None)`, not an error.
    pub fn base_pin(&self) -> io::Result<Option<BasePin>> {
        match fs::read_to_string(self.base_cache_dir().join(PIN_MARKER)) {
            Ok(s) => Ok(Some(BasePin::new(s.trim_end_matches('\n').to_string()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// **The single, audited path that mutates `base.cache/`.**
    ///
    /// Call this — and ONLY this — from an explicit git-advance op (after a
    /// `git pull` / `git rebase` on the base worktree advances the pinned
    /// state). In-flight `[dev]` edits must NEVER call this; they flow to
    /// `tree.cache/` via [`tree_cache_dir`](CacheLayout::tree_cache_dir). That
    /// asymmetry *is* the operator's decoupled-lifecycle guarantee — the
    /// reason rapid base edits don't re-derive across N active worktrees.
    ///
    /// Effect: records `pin` in `base.cache/PIN` atomically (temp + fsync +
    /// rename, the AC#4-never-publish-red write discipline — a torn pin marker
    /// would make a re-attaching daemon mis-judge cache validity). The actual
    /// re-derivation of pinned verdicts against the new base is the caller's
    /// (#8 corun / #6 cluster) concern; #7 owns the *gate*, not the recompute.
    ///
    /// # Errors
    /// Propagates an [`io::Error`] if `base.cache/` cannot be created or the
    /// marker cannot be written/synced/renamed.
    pub fn advance_base(&self, pin: &BasePin) -> io::Result<()> {
        let dir = self.base_cache_dir();
        fs::create_dir_all(&dir)?;
        let final_path = dir.join(PIN_MARKER);
        let mut contents = String::with_capacity(pin.as_str().len() + 1);
        contents.push_str(pin.as_str());
        contents.push('\n');
        atomic_write(&dir, &final_path, contents.as_bytes())
    }
}

/// The canonical, collision-free cache key for a *set* of overlay hashes.
///
/// Set semantics: the input slice is sorted and de-duplicated, so
/// `combined_key(&[A, B]) == combined_key(&[B, A]) == combined_key(&[A, A, B])`
/// — a corun batch is identified by *which worktrees are in it*, not the order
/// they were enumerated. The preimage is scheme-versioned and length-prefixed
/// (mirroring `cargoless-cas::identity::canonical_preimage`) so element-boundary
/// smear cannot alias distinct sets:
///
/// ```text
///   <scheme> <8-byte BE count> ( <8-byte BE len> <hash bytes> )…(sorted, deduped)
/// ```
///
/// Returns a 64-hex SHA-256 string suitable as a filename under `combined/`.
#[must_use]
pub fn combined_key(set: &[OverlayHash]) -> String {
    let mut sorted: Vec<&str> = set.iter().map(OverlayHash::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut buf = Vec::new();
    buf.extend_from_slice(COMBINED_SCHEME);
    buf.extend_from_slice(&(sorted.len() as u64).to_be_bytes());
    for h in sorted {
        let bytes = h.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        buf.extend_from_slice(bytes);
    }
    sha256_hex(&buf)
}

/// Atomic write: temp file in the *same directory* (so `rename` is atomic and
/// never crosses a filesystem boundary) + `fsync` the bytes + `rename` over the
/// final path. This is the AC#4 never-publish-red publisher discipline reused
/// for the base-pin marker; it also means #7 does not depend on (and is not
/// blocked by) the in-flight `cargoless-cas::LocalDiskStore::put` atomicity fix
/// — solo/combined entries are content-addressed write-once, and the only
/// mutable marker (`PIN`) carries its own atomic write here.
fn atomic_write(dir: &Path, final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let tmp = dir.join(format!(".{PIN_MARKER}.tmp.{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Don't leave a temp turd if the rename failed.
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cargoless-cache-layout-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Hash every regular file under `dir` into one order-independent digest.
    /// Used to assert `base.cache/` is *byte-unmoved* — the decoupled-lifecycle
    /// invariant, expressed as the same "byte-identical or it didn't move"
    /// discipline AC#4's never-publish-red uses.
    fn dir_fingerprint(dir: &Path) -> String {
        fn walk(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Ok(b) = fs::read(&p) {
                    out.push((p.to_string_lossy().into_owned(), b));
                }
            }
        }
        let mut entries = Vec::new();
        walk(dir, &mut entries);
        entries.sort();
        let mut buf = Vec::new();
        for (name, bytes) in entries {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0);
            buf.extend_from_slice(&bytes);
            buf.push(0);
        }
        sha256_hex(&buf)
    }

    #[test]
    fn layout_paths_are_the_design_5_shape() {
        let repo = scratch("paths");
        let l = CacheLayout::for_repo(&repo, TF_STATE_DIR_REL);

        assert_eq!(l.root(), repo.join(".triform/cargoless"));
        assert!(l.base_cache_dir().ends_with("base.cache"));
        assert!(l.tree_cache_dir().ends_with("tree.cache"));
        assert!(l.solo_dir().ends_with("solo"));
        assert!(l.combined_dir().ends_with("combined"));

        let hw = OverlayHash::new("a1b2c3");
        assert_eq!(l.solo_entry(&hw), l.solo_dir().join("a1b2c3"));

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn default_state_dir_is_backward_compat_and_tf_override_is_distinct() {
        let repo = scratch("statedir");
        let v0 = CacheLayout::for_repo(&repo, DEFAULT_STATE_DIR_REL);
        let tf = CacheLayout::for_repo(&repo, TF_STATE_DIR_REL);

        assert_eq!(v0.root(), repo.join(".cargoless"));
        assert_eq!(tf.root(), repo.join(".triform/cargoless"));
        assert_ne!(
            v0.root(),
            tf.root(),
            "the tf override must not alias the v0 backward-compat root"
        );

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn worktree_tree_cache_lives_under_the_worktree_not_the_base() {
        let repo = scratch("wt");
        let l = CacheLayout::for_repo(&repo, TF_STATE_DIR_REL);
        let wt = repo.join(".claude/worktrees/agent-x");

        let wtc = l.worktree_tree_cache(&wt);
        assert_eq!(wtc, wt.join(".triform/cargoless").join("tree.cache"));
        assert!(
            !wtc.starts_with(l.root()),
            "a worktree's tree.cache must NOT live under the base state root \
             (base advances must not be able to touch a worktree's cache)"
        );

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn combined_key_is_set_keyed_order_and_dup_independent() {
        let a = OverlayHash::new("aaaa");
        let b = OverlayHash::new("bbbb");
        let c = OverlayHash::new("cccc");

        let ab = combined_key(&[a.clone(), b.clone()]);
        let ba = combined_key(&[b.clone(), a.clone()]);
        let aab = combined_key(&[a.clone(), a.clone(), b.clone()]);
        assert_eq!(ab, ba, "{{A,B}} must equal {{B,A}} (set, not sequence)");
        assert_eq!(aab, ab, "{{A,A,B}} must equal {{A,B}} (deduped)");

        let abc = combined_key(&[a, b, c]);
        assert_ne!(
            abc, ab,
            "a different membership set must be a different key"
        );
        assert_eq!(ab.len(), 64, "combined key is a 64-hex SHA-256");
    }

    #[test]
    fn combined_key_resists_element_boundary_smear() {
        // The classic concatenation ambiguity, at the SET-element boundary:
        // { "ab", "c" } must NOT collide with { "a", "bc" }. Length-prefixing
        // the canonical preimage guarantees it — same property
        // cargoless-cas::identity proves for InputHash.
        let x = combined_key(&[OverlayHash::new("ab"), OverlayHash::new("c")]);
        let y = combined_key(&[OverlayHash::new("a"), OverlayHash::new("bc")]);
        assert_ne!(x, y, "element-boundary smear must not alias distinct sets");

        // And the empty set is its own stable, distinct key (not a panic, not
        // the same as any 1-element set).
        let empty = combined_key(&[]);
        assert_eq!(empty.len(), 64);
        assert_ne!(empty, combined_key(&[OverlayHash::new("")]));
    }

    #[test]
    fn solo_entry_is_content_addressed_stable() {
        let repo = scratch("solo");
        let l = CacheLayout::for_repo(&repo, TF_STATE_DIR_REL);
        let hw = OverlayHash::new("deadbeef");

        // Same overlay ⇒ same entry (cache hit); different overlay ⇒ different
        // entry, old one untouched (CAS immutability).
        assert_eq!(l.solo_entry(&hw), l.solo_entry(&hw));
        assert_ne!(l.solo_entry(&hw), l.solo_entry(&OverlayHash::new("cafe")));

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn advance_base_records_pin_atomically_and_is_readable() {
        let repo = scratch("advance");
        let l = CacheLayout::for_repo(&repo, TF_STATE_DIR_REL);
        l.ensure_dirs().unwrap();

        assert_eq!(l.base_pin().unwrap(), None, "fresh layout: never advanced");

        let pin1 = BasePin::new("49bb9c126");
        l.advance_base(&pin1).unwrap();
        assert_eq!(l.base_pin().unwrap(), Some(pin1));

        // A subsequent explicit git-advance moves the pin (and only the pin).
        let pin2 = BasePin::new("7f3a0011");
        l.advance_base(&pin2).unwrap();
        assert_eq!(l.base_pin().unwrap(), Some(pin2));

        // No temp turd left behind by the atomic write.
        let leftover: Vec<_> = fs::read_dir(l.base_cache_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "atomic_write must not leak temp files");

        let _ = fs::remove_dir_all(&repo);
    }

    /// The launch-critical regression guard for the operator's
    /// decoupled-lifecycle insight: a storm of in-flight writes to
    /// `tree.cache/`, `solo/`, and `combined/` must leave `base.cache/`
    /// **byte-identical**. Only `advance_base` may move it.
    #[test]
    fn base_cache_is_byte_unmoved_by_inflight_writes() {
        let repo = scratch("decoupled");
        let l = CacheLayout::for_repo(&repo, TF_STATE_DIR_REL);
        l.ensure_dirs().unwrap();

        // Establish a pinned base (one explicit git-advance).
        l.advance_base(&BasePin::new("base-rev-0001")).unwrap();
        let pinned = dir_fingerprint(&l.base_cache_dir());

        // Now simulate heavy in-flight activity through EVERY non-base path:
        // base [dev]'s own edits → tree.cache; per-WT solo verdicts; corun
        // combined verdicts; a non-base worktree's tree.cache.
        for i in 0..50 {
            let tc = l.tree_cache_dir();
            fs::create_dir_all(&tc).unwrap();
            fs::write(tc.join(format!("inflight-{i}")), format!("edit {i}")).unwrap();

            let hw = OverlayHash::new(format!("hw-{i:04}"));
            let se = l.solo_entry(&hw);
            fs::create_dir_all(se.parent().unwrap()).unwrap();
            fs::write(&se, format!("solo verdict {i}")).unwrap();

            let ce = l.combined_entry(&[hw.clone(), OverlayHash::new("peer")]);
            fs::create_dir_all(ce.parent().unwrap()).unwrap();
            fs::write(&ce, format!("combined verdict {i}")).unwrap();

            let wtc = l.worktree_tree_cache(repo.join(format!(".claude/worktrees/w{i}")));
            fs::create_dir_all(&wtc).unwrap();
            fs::write(wtc.join("cli-status"), format!("verdict={i}")).unwrap();
        }

        assert_eq!(
            pinned,
            dir_fingerprint(&l.base_cache_dir()),
            "base.cache/ MUST be byte-unmoved by in-flight tree/solo/combined \
             writes — this is the operator's decoupled-lifecycle invariant; \
             only advance_base() (an explicit git-advance op) may move it"
        );

        // And the gate still works after the storm: an explicit advance DOES
        // move it (proving the guard above isn't vacuously true).
        l.advance_base(&BasePin::new("base-rev-0002")).unwrap();
        assert_ne!(
            pinned,
            dir_fingerprint(&l.base_cache_dir()),
            "advance_base IS the one path that moves base.cache"
        );

        let _ = fs::remove_dir_all(&repo);
    }
}
