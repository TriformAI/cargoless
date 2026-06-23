# Known-blind corpus (#A8)

Files in this directory reproduce diagnostic classes the daemon's
RA-native verdict is KNOWN to miss — errors that only exist after
proc-macro expansion. Both classes are documented from tf-multiverse
incident #4070 (a PR shipped cargoless-GREEN, broke `cargo build`, and
needed #4078 plus a manual skip override to land the fix).

**They are deliberately NOT referenced by any `mod` declaration.** Cargo
only compiles modules reachable from the crate root, so the fixture's
`cargo check` / `cargo build` (the bench CI job) stay green while these
files sit here. Do NOT add `mod known_blind;` anywhere — each file fails
`cargo build` BY DESIGN; that is exactly what makes the class
witness-only.

| File | Class | Expected compiler error |
|---|---|---|
| `early_return_into_any.rs` | early-return `view!` branch not unified with the tail branch (missing `.into_any()`) | E0308 mismatched types |
| `double_capture_move.rs` | same variable captured twice inside one `view!` without an intermediate `let` | E0382 use of moved value |

Transcribed from the incident write-up (tf-mv `portal/CLAUDE.md`); the
portal runs a newer Leptos than this fixture pins (`=0.6.15`), so exact
error wording may differ here. The corpus is (a) documentation-as-code
of the blind classes behind `CARGOLESS_MACRO_BLIND_PATHS`, and (b) the
target set for a future S1-harness mode that injects each file into the
module tree and asserts the daemon publishes `ra_blind_paths: true` —
and, under `CARGOLESS_MACRO_BLIND_ESCALATE=1`, a witness-backed verdict
instead of an RA-native green.

A third, *non-macro* blind class also rides `ra_blind_paths`: **cross-crate
type resolution** in a generated twin — a `generated/ui-frozen/*.rs`
referencing an unimported type (`cannot find type DonutSlice`, E0425), which
RA-native greened and a later rustc/SSR compile caught. It is covered by the
content-exempt `CARGOLESS_BLIND_PATHS` glob set (always blind, no `view!` for
a content scan to key on), not `CARGOLESS_MACRO_BLIND_PATHS` — see
`docs/design/D-PROJECT-CHECKS.md` § Blind-path coverage. A fixture for this
class is not added here (it would need a sibling crate to import from, unlike
the self-contained macro-expansion files above).
