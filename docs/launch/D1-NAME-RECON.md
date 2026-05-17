# D1 — product name recon

> **Status:** evidence-bundle for operator decision (Plane CWDL-12 / D-RELEASE
> §8 #1). Author: `docs-launch-lead` (2026-05-17). Reviewer:
> `team-lead` → operator. Outputs ranked shortlist with verifiable
> availability evidence; the operator picks the final name.

---

## TL;DR

**`cargoless` is the only candidate uniquely free across all four registries
checked** (crates.io / GitHub / npm / pypi). Every other candidate has at
least one substantive collision, and several have *direct adjacent-ecosystem
collisions* (a Rust dev-tool with the same name) — e.g. `steward` is itself
a Rust task-runner, `cadence` is an active Rust StatsD client, `lumen` is a
Rust BEAM-VM project that compiles to WebAssembly (cargoless's exact audience).

**Recommended top-3 for operator decision, in priority order:**

1. **`cargoless`** — incumbent advantage + uniquely free. Total score 21/25.
   Mild risk: the name *describes a feature* (CAS-dedupe means "the build
   is cargo-less on no-op edits") which the user might not parse without
   context. Mitigation: README hook explains it in two sentences.
2. **`crisp`** — best Framing-C-aligned alternative. Total 20/25. Rust
   collision (Crisp is a Lisp-like language) is in a clearly separate
   domain; main risk is the **Crisp IM brand** (chat/customer-service
   SaaS). Trademark check recommended.
3. **`pulse`** — strong continuous-heartbeat evocation. Total 19/25. Rust
   collision is **`libpulse-binding`** (PulseAudio bindings) which is
   adjacent enough to confuse — would force the publish name to a
   variant like `pulse-rs` or `cargo-pulse`.

If the operator's intuition is "let's go with the working name" — the
evidence supports that intuition strongly. The 8 alternatives below exist
because the brief asked for 8-12 candidates with evidence, not because the
incumbent is in doubt.

---

## Methodology

For each candidate, 4 verifiable availability checks + 5 subjective scores +
ecosystem-collision context.

**Availability checks** (live API queries, 2026-05-17):

- **crates.io**: `curl -A "<UA>" -s -o /dev/null -w "%{http_code}" https://crates.io/api/v1/crates/<name>` — 200 = TAKEN, 404 = AVAILABLE.
- **GitHub user/org**: `curl -s -o /dev/null -w "%{http_code}" https://api.github.com/users/<name>` — 200 = TAKEN, 404 = AVAILABLE.
- **npm**: `curl -s -o /dev/null -w "%{http_code}" https://registry.npmjs.org/<name>` — 200 = TAKEN, 404 = AVAILABLE.
- **pypi**: `curl -s -o /dev/null -w "%{http_code}" https://pypi.org/pypi/<name>/json` — 200 = TAKEN, 404 = AVAILABLE.

Note: a *taken* registry name is not automatically a *substantive* collision;
many names are squatted by abandoned 0.0.1 placeholders. The
ecosystem-collision column distinguishes substantive (active project) from
nominal (placeholder) collisions.

**Subjective scoring** (1-5 each, 25 max):

- **P** (Pronounceability) — does a stranger reading it aloud get it right?
- **M** (Memorability) — does it stick after one read?
- **S** (Searchability) — does `"<name> rust"` return useful first-page
  results, or does noise drown the project?
- **E** (Evocativeness) — does it suggest the vision (always-knows /
  continuous / throughput / honesty)?
- **C** (Cool-factor) — does it read like something a Rust dev wants to
  type at the terminal?

**Constraints applied** (per CLAUDE.md + D-RELEASE §8 #1):

- 5-10 chars preferred, max ~12.
- NOT a Terraform-collision (no `tf-*`, `terra-*`, `hashi-*` — the working
  `tf-*` placeholder names are why D1 exists).
- NOT a known Rust dev-tool collision (`bacon`, `trunk`, `cargo-watch`,
  `watchexec`, `cargo-make`, `just`, `cranelift`, `salsa`).
- NOT rust-analyzer-adjacent (`analyzer`, `ra-*`, `rust-*`).
- HARD: free on crates.io + GitHub (the registry-level gating).
- Evocative of the vision.

---

## Candidate matrix (availability + scores)

| # | Name | Chars | crates.io | GitHub | npm | pypi | P | M | S | E | C | **Total** | Ecosystem-collision severity |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | **cargoless** | 9 | ✅ FREE | ✅ FREE | ✅ FREE | ✅ FREE | 4 | 4 | 5 | 4 | 4 | **21** | **NONE** (uniquely clear) |
| 2 | **crisp** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 5 | 5 | 2 | 4 | 4 | **20** | MEDIUM-HIGH (Lisp lang + Crisp IM brand) |
| 3 | **pulse** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 5 | 5 | 1 | 4 | 4 | **19** | HIGH (libpulse / PulseAudio dominates) |
| 4 | **mercury** | 7 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 5 | 5 | 1 | 3 | 4 | **18** | HIGH (4+ Rust crates + Mercury Bank brand) |
| 5 | **verity** | 6 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 4 | 4 | 2 | 4 | 3 | **17** | MEDIUM (`fs-verity` Linux kernel verification) |
| 6 | **cadence** | 7 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ✅ FREE | 4 | 4 | 1 | 4 | 4 | **17** | HIGH (`cadence` is an active Rust StatsD client) |
| 7 | **vigil** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 4 | 4 | 2 | 4 | 3 | **17** | HIGH (`vigil` Rust status-page; vigil-pulse/server/local) |
| 8 | **gauge** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 5 | 4 | 1 | 4 | 3 | **17** | HIGH (Gauge test framework + Prometheus gauge type) |
| 9 | **steward** | 7 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 5 | 4 | 1 | 4 | 3 | **17** | **VERY HIGH** (ShakaCode `steward` IS a Rust task-runner / process-manager — direct adjacent collision) |
| 10 | **augur** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 3 | 4 | 3 | 4 | 3 | **17** | LOW-MED (small Rust crate; Augur prediction-market cultural noise) |
| 11 | **plumb** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 3 | 3 | 3 | 4 | 3 | **16** | LOW-MED (small Rust crate; "Rust the game" pollutes search) |
| 12 | **lumen** | 5 | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | ❌ TAKEN | 4 | 4 | 1 | 3 | 4 | **16** | **VERY HIGH** (Lumen project compiles Erlang/BEAM to WebAssembly — directly in cargoless's audience space) |

---

## Per-candidate detail

### 1. `cargoless` — the incumbent (RECOMMENDED) — total 21/25

- **Availability:** UNIQUELY FREE on all four registries. No collision to negotiate.
- **Vision fit:** the name *is* a feature description. CAS dedupe means many save cycles never invoke `cargo` at all — the build genuinely is "cargo-less" on no-op edits. This maps to Framing C (throughput / "doesn't burn your CPU").
- **Risks:** (a) people may parse it as "without cargo" and ask "wait, isn't this a Rust project?" — but the README hook resolves that in two sentences. (b) Slightly long at 9 chars; not as punchy as a 5-char name.
- **Adoption cost:** ZERO. 417 references in the codebase already use this name; no rename sweep needed.
- **Evidence:** crates.io 404, GitHub user 404, npm 404, pypi 404 (all confirmed 2026-05-17 via direct API).

### 2. `crisp` — best Framing-C-aligned alternative — total 20/25

- **Availability:** crates.io TAKEN; GitHub user TAKEN; npm TAKEN; pypi TAKEN. The bare `crisp` crate on crates.io is a **Lisp-like programming language** ([crates.io/crates/crisp](https://crates.io/crates/crisp)).
- **Vision fit:** "Crisp" suggests efficient, clean, no-waste — directly Framing-C-coded. Short and punchy (5 chars). High pronounceability, high memorability.
- **Risks:** (a) The Rust `crisp` crate is a language implementation — a Rust developer searching "crisp rust" finds a *language*, not a tool. (b) Bigger risk: **Crisp IM SAS** is a real company (customer-chat SaaS) — trademark check required before commitment.
- **Mitigation if picked:** publish as `crisp-cli` or `crisp-rs` on crates.io; brand as the bare name in marketing. Verify Crisp IM trademark scope (likely fine since they're in a different category, but explicit clearance is the operator's risk to take).

### 3. `pulse` — strong continuous-heartbeat fit — total 19/25

- **Availability:** TAKEN everywhere. Bare `pulse` on crates.io is an **async wake-signals concurrency library** (9 years old, low activity). However, **`libpulse-binding`** (PulseAudio bindings) is the active ecosystem citizen and dominates the `pulse rust` search namespace.
- **Vision fit:** "pulse" suggests continuous-heartbeat, live signal, always-on — direct evocation of the watch-loop verdict stream.
- **Risks:** PulseAudio bindings will dominate searches and lead to user confusion ("is this an audio library?"). Searchability suffers heavily.
- **Mitigation if picked:** publish as `pulse-rs`, `cargo-pulse`, or `pulse-dev`; brand decision needed.

### 4. `mercury` — quicksilver evocation — total 18/25

- **Availability:** TAKEN on all four. **At least 4 distinct Rust crates** named mercury / mercury-rust / mercury-cli (Mercury Parser client, Mercury Bank API client, Internet-of-People P2P network, Webex Mercury WebSocket).
- **Vision fit:** quicksilver / always-flowing / fast-and-fluid. Decent.
- **Risks:** crowded namespace; trademark concerns from **Mercury Bank** (active financial brand). Cultural noise from the planet, the element, and Freddie Mercury.
- **Verdict:** strong name but the ecosystem is too crowded for a clean win.

### 5. `verity` — honesty/truth — total 17/25

- **Availability:** TAKEN. Main collision is **`fs-verity`** ([crates.io/crates/fs-verity](https://crates.io/crates/fs-verity)) — Linux kernel filesystem-integrity verification.
- **Vision fit:** "always knows what's true" — vision-aligned. Latin root reads as classical/serious.
- **Risks:** `fs-verity` is in the *verification / integrity* space, which is conceptually adjacent (different mechanism, similar word-frame). Search results will conflate them.
- **Verdict:** good name in a vacuum; collision is borderline-substantive.

### 6. `cadence` — continuous-rhythm — total 17/25

- **Availability:** TAKEN on 3 of 4 (pypi free). The crates.io `cadence` is an **active, well-known Rust StatsD client** ([crates.io/crates/cadence](https://crates.io/crates/cadence)) — emits metrics over UDP/Unix sockets, used in production by many Rust services.
- **Vision fit:** rhythm/continuous-tempo — good for the watch-loop framing.
- **Risks:** DIRECT collision with an active observability tool. A dev searching `cadence rust` lands on the metrics library first.
- **Verdict:** would force a name like `cadence-dev` or `cadence-build`; not clean.

### 7. `vigil` — continuous-watch — total 17/25

- **Availability:** TAKEN. Collisions are heavy: **`vigil` is a prominent Rust open-source status-page tool** ([github.com/valeriansaliou/vigil](https://github.com/valeriansaliou/vigil), HN-trending), with companion crates `vigil-pulse`, `vigil-server`, `vigil-local`. There's also a Metaswitch `vigil` software-watchdog crate.
- **Vision fit:** continuous-watch / always-watching is a good frame.
- **Risks:** the existing `vigil` is a monitoring/observability tool — directly adjacent in mental model. Confusion is likely.

### 8. `gauge` — measurement-of-truth — total 17/25

- **Availability:** TAKEN everywhere. **Gauge** is a well-known cross-platform **test automation framework** ([github.com/getgauge/gauge](https://github.com/getgauge/gauge)) with a Rust language plugin. Also collides with Prometheus' Gauge metric type.
- **Vision fit:** measures the truth state of the codebase.
- **Risks:** Gauge-the-test-framework is in the dev-tooling space — direct ecosystem collision.

### 9. `steward` — caretaker — total 17/25

- **Availability:** TAKEN everywhere. **The `steward` crate IS a Rust task-runner and process manager by ShakaCode** ([github.com/shakacode/steward](https://github.com/shakacode/steward), [crates.io/crates/steward](https://crates.io/crates/steward)) — *the exact category cargoless competes in*. Their tagline: "Task runner and process manager for Rust... great with clap, good fit for building CLI tools."
- **Vision fit:** caretaker of the codebase. Vision-aligned.
- **Risks:** **maximum-severity adjacent collision.** Two Rust task-management CLI tools named `steward` would be immediately confusing to the audience.
- **Verdict:** **strongly discouraged.** Pick something else.

### 10. `augur` — predicts-the-truth — total 17/25

- **Availability:** TAKEN. crates.io `augur` is a **reverse-engineering IDA Pro plugin** ([crates.io/crates/augur](https://crates.io/crates/augur)) — low Rust-ecosystem footprint. `augurs` (with trailing s) is a Grafana time-series toolkit.
- **Vision fit:** "augur" = one who tells the truth from signs; matches "always knows what works" framing nicely.
- **Risks:** cultural collision with **Augur** the crypto prediction-market platform (Ethereum-based; was prominent c. 2018, now less visible). Pronunciation slightly ambiguous (auger vs augur).
- **Verdict:** a viable alternative if the operator wants a non-incumbent option with low Rust-ecosystem collision substance.

### 11. `plumb` — true/level — total 16/25

- **Availability:** TAKEN. crates.io `plumb` is a small 0.2.0 library; `plumbing` is a small async-pipelining lib. The "Rust" search-term pollution comes from **Rust the survival video game** (in-game pipe tool) — not Rust-the-language.
- **Vision fit:** "plumb-line of truth" is a nice metaphor; novel.
- **Risks:** "plumb" rhymes with "dumb" — slight cool-factor penalty. Heavy game-domain search noise.
- **Verdict:** clever but not punchy enough; ecosystem collision is low but the name doesn't *click*.

### 12. `lumen` — light/lens — total 16/25

- **Availability:** TAKEN. Multiple collisions, but the critical one: **the Lumen project compiles Erlang/BEAM to WebAssembly** ([lumen.dev](https://underjord.io/lumen-elixir-in-the-browser.html); since renamed to Firefly). That's **directly in cargoless's audience space** (Rust + WASM). Plus the Laravel Lumen PHP microframework. Plus `lumen-language`, `oalacea-lumen`, etc.
- **Vision fit:** light / lens / illumination — abstract.
- **Risks:** **VERY HIGH** — the most directly-adjacent ecosystem collision of any candidate. A Rust+WASM developer searching "lumen rust wasm" gets a confusing tangle.
- **Verdict:** **avoid.** Too much overlap with the actual audience.

---

## Recommendation for operator

**Strong default: `cargoless`.** The evidence (uniquely free across four
registries, zero rename cost, 417 in-codebase references already
self-consistent, name-as-feature-description aligns with Framing C
"doesn't burn your CPU" via the CAS-dedupe mechanism) is hard to beat.

**If the operator wants a non-incumbent option** (perhaps because
"cargoless" reads as describing what the tool *isn't* rather than what
it *is*), the realistic alternatives are:

| Pick | Why | Operator decision needed |
|---|---|---|
| `crisp` | Framing-C-aligned, punchy 5 chars | Trademark check vs Crisp IM SAS |
| `augur` | Low Rust-ecosystem collision, vision-fitting | Cultural risk: Augur prediction-market association |

**Strongly discouraged:** `steward` (adjacent-ecosystem collision is
maximum-severity), `lumen` (literally compiles to WASM — cargoless's
audience), `cadence` (active StatsD client dominates the name).

**If the operator wants more options**, the obvious next moves are:
- Generate a second round of 8-12 candidates from a *different* theme
  bucket (e.g., made-up portmanteaus, classical-mythology figures,
  geographic/place-based names) — the current 12 covered classical /
  honesty / continuous / light themes.
- Allow longer/compound names (e.g., `coolforge`, `truebeat`) which
  trade memorability for availability.

The recon is complete. Operator picks.

---

## Sources

Live API queries (2026-05-17):

- crates.io API ([crates.io/api/v1/crates/...](https://crates.io/api))
- GitHub user/org API ([api.github.com/users/...](https://docs.github.com/en/rest))
- npm registry ([registry.npmjs.org](https://registry.npmjs.org))
- PyPI JSON API ([pypi.org/pypi/.../json](https://pypi.org))

Ecosystem-collision research (web search per candidate, 2026-05-17):

- [fs-verity crate](https://crates.io/crates/fs-verity), [verity crate](https://crates.io/crates/verity), [verity-client](https://crates.io/crates/verity-client)
- [cadence crate](https://crates.io/crates/cadence), [56quarters/cadence](https://github.com/56quarters/cadence)
- [plumb crate](https://docs.rs/plumb/0.2.0/plumb/), [plumbing crate](https://docs.rs/plumbing)
- [Lumen ShareX uploader](https://github.com/ChecksumDev/lumen), [Lumen BEAM/WASM project](https://underjord.io/lumen-elixir-in-the-browser.html), [lumen crate](https://crates.io/crates/lumen)
- [Vigil status page](https://github.com/valeriansaliou/vigil), [Metaswitch Vigil watchdog](https://github.com/Metaswitch/Vigil), [vigil crate](https://crates.io/crates/vigil)
- [mercury crate](https://crates.io/crates/mercury), [postlight/mercury-rs](https://github.com/postlight/mercury-rs), [Internet-of-People/mercury-rust](https://github.com/Internet-of-People/mercury-rust)
- [Gauge test framework](https://github.com/getgauge/gauge), [gauge-rust plugin](https://github.com/getgauge-contrib/gauge-rust)
- [steward crate](https://crates.io/crates/steward), [shakacode/steward](https://github.com/shakacode/steward)
- [crisp crate](https://crates.io/crates/crisp), [Crisp IM Status Local](https://crates.io/crates/crisp-status-local)
- [libpulse-binding](https://crates.io/crates/libpulse-binding), [pulse crate](https://crates.io/crates/pulse)
- [augur crate](https://crates.io/crates/augur), [augurs time-series toolkit](https://crates.io/crates/augurs), [0xdea/augur](https://github.com/0xdea/augur)
