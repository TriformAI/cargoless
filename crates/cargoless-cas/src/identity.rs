//! The `BuildIdentity → InputHash` reduction — the CAS key derivation.
//!
//! `cargoless-proto` carries the *shape* of a build's identity (each output-affecting
//! input as its own typed field) and guarantees one invariant to every
//! consumer: **equal [`BuildIdentity`] ⇒ equal [`InputHash`] ⇒ substitutable
//! artifact** (AC#5 dedupe, AC#4 provenance). It deliberately does *not*
//! specify how the reduction is computed — that is this crate's job. This
//! module is the single, canonical implementation.
//!
//! ## Why a length-prefixed canonical encoding
//!
//! A naive `hash(a + b + c)` is ambiguous: `("ab","c")` and `("a","bc")`
//! collide. A collision here is not a hash weakness — it is a *wrong-artifact*
//! bug (serve build X for input set Y). So every component is encoded as
//!
//! ```text
//!   <1-byte field tag> <8-byte big-endian length> <raw bytes>
//! ```
//!
//! preceded by a scheme-version line. Distinct `BuildIdentity` values cannot
//! share a preimage, and the scheme version makes any future change to the
//! reduction an explicit, observable cache-space move rather than a silent
//! one.

use cargoless_proto::{BuildIdentity, ContentHash, InputHash};

use crate::sha256::sha256_hex;

/// Bumping this string is a deliberate, repo-visible decision to move the
/// entire CAS key space (every prior cached artifact becomes unreachable).
/// It is part of the hashed preimage so the move can never be silent.
const SCHEME: &[u8] = b"tf-cas/input-hash/v1";

// Field tags — one per `BuildIdentity` component. Never reorder or reuse a
// value: that would alias historically-distinct identities.
const TAG_SOURCE_TREE: u8 = 0x01;
const TAG_CARGO_LOCK: u8 = 0x02;
const TAG_RUST_TOOLCHAIN: u8 = 0x03;
const TAG_TF_CONFIG: u8 = 0x04;
const TAG_TARGET: u8 = 0x05;
const TAG_PROFILE: u8 = 0x06;

fn push_field(buf: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    buf.push(tag);
    buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// The canonical, unambiguous byte preimage of a [`BuildIdentity`].
///
/// Exposed (crate-internal) so the determinism tests can assert the *encoding*
/// is injective independently of the hash function.
fn canonical_preimage(id: &BuildIdentity) -> Vec<u8> {
    let mut buf = Vec::new();
    push_field(&mut buf, 0x00, SCHEME);
    push_field(
        &mut buf,
        TAG_SOURCE_TREE,
        id.source_tree.as_str().as_bytes(),
    );
    push_field(&mut buf, TAG_CARGO_LOCK, id.cargo_lock.as_str().as_bytes());
    push_field(
        &mut buf,
        TAG_RUST_TOOLCHAIN,
        id.rust_toolchain.as_str().as_bytes(),
    );
    push_field(&mut buf, TAG_TF_CONFIG, id.tf_config.as_str().as_bytes());
    push_field(&mut buf, TAG_TARGET, id.target.as_str().as_bytes());
    push_field(&mut buf, TAG_PROFILE, id.profile.as_str().as_bytes());
    buf
}

/// Reduce a full [`BuildIdentity`] to its single [`InputHash`] CAS key.
///
/// This is *the* function behind the `cargoless-proto` invariant. It is total,
/// deterministic, and dependency-free. Two `BuildIdentity` values compare
/// `Eq` **iff** this returns equal `InputHash` values — proven component by
/// component in [`tests`].
#[must_use]
pub fn input_hash(identity: &BuildIdentity) -> InputHash {
    InputHash::new(sha256_hex(&canonical_preimage(identity)))
}

/// Hash an arbitrary input file's content into a [`ContentHash`] (the type the
/// daemon uses to fill `BuildIdentity` component fields). One helper so every
/// component is hashed by the *same* primitive the CAS key uses.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> ContentHash {
    ContentHash::new(sha256_hex(bytes))
}

/// The stable marker hashed in place of an absent optional input
/// (`Cargo.lock`, `rust-toolchain.toml`, `tf.toml`). "File absent" must be a
/// *distinct, deterministic* state — never confusable with an empty file
/// (`sha256("")`) — or adding/removing the file would not invalidate the
/// cache. Per-kind tag keeps "no lock" ≠ "no config".
#[must_use]
pub fn absent_marker(kind: &str) -> ContentHash {
    let mut v = Vec::with_capacity(16 + kind.len());
    v.extend_from_slice(b"tf-cas/absent:");
    v.extend_from_slice(kind.as_bytes());
    ContentHash::new(sha256_hex(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargoless_proto::{Profile, TargetTriple};

    fn base() -> BuildIdentity {
        BuildIdentity {
            source_tree: ContentHash::new("src-aaaa"),
            cargo_lock: ContentHash::new("lock-bbbb"),
            rust_toolchain: ContentHash::new("tc-cccc"),
            tf_config: ContentHash::new("cfg-dddd"),
            target: TargetTriple::new("wasm32-unknown-unknown"),
            profile: Profile::Dev,
        }
    }

    #[test]
    fn equal_identity_yields_equal_hash() {
        assert_eq!(
            input_hash(&base()),
            input_hash(&base()),
            "the cargoless-proto invariant: equal BuildIdentity ⇒ equal InputHash"
        );
        // And it is a real 64-hex SHA-256, not a passthrough.
        assert_eq!(input_hash(&base()).as_str().len(), 64);
    }

    #[test]
    fn every_single_component_change_changes_the_key() {
        let base = base();
        let h = input_hash(&base);

        let mut a = base.clone();
        a.source_tree = ContentHash::new("src-ZZZZ");
        assert_ne!(h, input_hash(&a), "source change must invalidate");

        let mut b = base.clone();
        b.cargo_lock = ContentHash::new("lock-ZZZZ");
        assert_ne!(h, input_hash(&b), "dependency graph change must invalidate");

        let mut c = base.clone();
        c.rust_toolchain = ContentHash::new("tc-ZZZZ");
        assert_ne!(h, input_hash(&c), "toolchain bump must invalidate");

        let mut d = base.clone();
        d.tf_config = ContentHash::new("cfg-ZZZZ");
        assert_ne!(h, input_hash(&d), "config change must invalidate");

        let mut e = base.clone();
        e.target = TargetTriple::new("x86_64-unknown-linux-gnu");
        assert_ne!(h, input_hash(&e), "target triple must invalidate");

        let mut f = base.clone();
        f.profile = Profile::Release;
        assert_ne!(
            h,
            input_hash(&f),
            "a release build must never alias a dev artifact"
        );
    }

    #[test]
    fn encoding_is_injective_against_field_smear() {
        // The classic concatenation ambiguity: moving a character across a
        // field boundary must change the key. Length-prefixing guarantees it.
        let mut x = base();
        x.cargo_lock = ContentHash::new("ab");
        x.rust_toolchain = ContentHash::new("c");

        let mut y = base();
        y.cargo_lock = ContentHash::new("a");
        y.rust_toolchain = ContentHash::new("bc");

        assert_ne!(
            input_hash(&x),
            input_hash(&y),
            "field smear must not collide (length-prefixed preimage)"
        );
        assert_ne!(canonical_preimage(&x), canonical_preimage(&y));
    }

    #[test]
    fn absent_marker_is_distinct_from_empty_and_per_kind() {
        assert_ne!(absent_marker("cargo_lock").as_str(), sha256_hex(b""));
        assert_ne!(
            absent_marker("cargo_lock").as_str(),
            absent_marker("tf_config").as_str(),
            "absent lock and absent config must not alias"
        );
        assert_eq!(
            absent_marker("tf_config"),
            absent_marker("tf_config"),
            "absent marker is deterministic"
        );
    }

    #[test]
    fn content_hash_uses_the_same_primitive() {
        assert_eq!(content_hash(b"abc").as_str(), sha256_hex(b"abc"));
    }
}
