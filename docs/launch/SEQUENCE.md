# Launch sequence — v0 public announcement plan

**Status:** DRAFT. Approved as a working plan; venue-by-venue go/no-go
decisions remain with the operator. Spacing is the spine; the venues
listed are the obvious-first-pass, not an exhaustive enumeration.

**Owner:** team-lead (operator-facing); docs-launch-lead authors and
maintains this document; outside reviewer per AC#9 reviews the venue
selection alongside the blog draft.

**Pre-requirement (hard gate):** None of these venues fire until
**ALL** of the following have landed:

1. D1 product name resolved + crates renamed + crates.io reserved.
2. `0.1.0` semver tag pushed + GitHub release page populated with the
   three prebuilt tarballs + SHA-256 sums.
3. `cargo install <pubname>` and `cargo binstall <pubname>` smoke-tested
   from a clean machine on at least one supported platform.
4. AC#9 launch blog post reviewed by ≥2 (incl. one outside the team).
5. README finalized with the actual numbers from the AC#7 bench and the
   operator-locked positioning framing — no TBD-NUMBERS or
   TBD-POSITIONING placeholders left in any published copy.
6. Post-launch responsiveness commitment in [`CONTRIBUTING.md`](../../CONTRIBUTING.md)
   reflects the actual maintainer roster + on-call window for the
   launch fortnight.

---

## Why sequence matters

A v0-of-a-trust-product launch has one rehearsable shot at first
impressions. Hitting every venue simultaneously produces:

- **Synchronized firefighting:** if launch-day bugs surface (and they
  will), maintainers triage across 5 channels at once, drop
  responsiveness on at least 3 of them, and the "alive and listening"
  signal collapses for the audience that found us via the dropped
  venues.
- **No cross-channel echo.** A TWiR mention seen 24h later by an
  r/rust browser reads as "this is real and propagating"; the same
  pair seen simultaneously reads as a coordinated PR push.
- **A single bad early reaction sets the tone everywhere.** If the
  first HN comment is "this is just `bacon`-with-extra-steps", that
  framing colours every other venue's reception in the next 48h. A
  staggered launch lets you respond to that reaction on HN, then
  enter the next venue with a hardened pitch.

The sequencing principle: **one major venue at a time, 1-2 days apart,
soft venues before loud venues.** Soft = curated/scoped audiences who
will read carefully (TWiR, Leptos Discord). Loud = broad mass-audience
(HN, Lobsters, r/rust). Soft venues filter early bug-reports and
calibrate positioning before the loud venues amplify.

---

## The venue sequence

### 1. This Week in Rust (TWiR) — **essential, weekly cadence**

- **Timing:** submit to the next open issue, aim to land on a
  Wednesday (TWiR publishes Wednesdays UTC).
- **Format:** PR to
  [rust-lang/this-week-in-rust](https://github.com/rust-lang/this-week-in-rust),
  one-paragraph entry under "Crate of the Week" or "Updates from Rust
  Community" → "Newsletters / Tooling".
- **Why first:** TWiR is the project's calibration shot. It's a
  high-signal, low-noise audience of active Rust developers who read
  carefully, file substantive issues, and rarely flame. Early field
  feedback here lets us harden before the louder venues.
- **What to write:** 2-3 sentences max — vision claim, one specific
  capability, install command, link to README. No fluff.
- **Risk:** very low. TWiR mentions rarely produce traffic spikes that
  overload a project.

### 2. r/rust — 1-2 days after TWiR

- **Timing:** Thursday or Friday after TWiR Wednesday. Avoid Mondays
  (low engagement) and weekend evenings (post buried by Sunday-night
  threads).
- **Format:** self-post on
  [r/rust](https://www.reddit.com/r/rust/), title = headline value-prop
  + project name (`<pubname>: <one-line vision claim>`). Body =
  paragraph hook + 3-5 bullet "what this is / isn't", install command,
  link to GitHub + blog post.
- **Why second:** r/rust is broader than TWiR but still
  Rust-specific. The audience appreciates honest framing and reacts
  badly to marketing-fluff or speed-claims-without-evidence (relevant
  to our AC#7 INCONCLUSIVE positioning). Lands better with the blog
  post live to link to.
- **What to monitor:** top-comment hostility. The thread's first
  3 hours determine the day's reception. Maintainer-or-on-call replies
  to substantive critique within 60 minutes.
- **Risk:** medium. r/rust can rapidly reframe a project around its
  weakest claim; an honest "what cargoless deliberately does not do"
  paragraph in the body inoculates against most of this.

### 3. Leptos Discord (#announcements or #ecosystem) — same day or +1 from r/rust

- **Timing:** post in the Leptos Discord
  [#announcements channel](https://discord.gg/YdRAhS7eQB) after the r/rust
  thread settles. Soft venue with an aligned audience (cargoless's
  Leptos-first defaults are a real fit there per
  [DESIGN.md D2](../../docs/DESIGN.md#d2--audience-wedge-leptos-first-vs-broad-rustwasm)).
- **Format:** 2-paragraph message + GitHub link + install command +
  blog link.
- **Why third:** Leptos community is the **highest-signal early
  adopter pool** for cargoless's specific defaults. They have real
  Leptos projects to dogfood against, and a positive reception here
  produces "I tried it on my project and it works" testimony that
  legitimizes the louder venues.
- **What to monitor:** Discord engagement is hard to measure but
  high-quality. Pin any substantive feedback to the maintainer
  on-call rotation.
- **Risk:** low. Aligned audience.

### 4. Hacker News — **coin-flip on timing**, +1 to +3 days after Discord

- **Timing:** the operator coin-flips between two options:
  - **(a) Same-day-as-Discord HN submission**, to amplify the
    cross-channel echo while community sentiment is fresh.
  - **(b) Wait 2-3 days, gauge initial reception, then submit a
    revised pitch** that incorporates the early field feedback.
  Default to **(b)** unless the TWiR + r/rust + Discord cycle
  produces overwhelmingly positive signal, in which case (a)
  captures the momentum.
- **Format:** "Show HN: cargoless – the codebase always knows what
  works" (or the operator-final framing). Link to the GitHub
  repository (NOT the blog post — HN downranks blog links). First
  comment by the OP (operator or on-call maintainer) within 15
  minutes, framing what the project is and what it isn't.
- **Why fourth:** HN is the highest-amplification, highest-volatility
  venue. A front-page HN can produce a 100x traffic spike and
  10-100 substantive comments in the first day. **Maintainer
  bandwidth must be deliberately reserved** for the 24h post-submission
  window — this is the launch's most demanding day.
- **Submission timing within the day:** aim for 14:00-16:00 UTC
  (morning US east coast, afternoon Europe; statistically the most
  active reader window for technical Show-HN posts).
- **What to monitor:** front-page ranking via
  [hnrankings.info](https://hnrankings.info/) or similar; reply to
  every substantive top-level comment within 60 minutes for the first
  6 hours. Expect at least one "why not just use trunk/bacon" comment
  — the response is the same honest framing as the blog's
  problem-statement section.
- **Risk:** highest. HN can either be the launch's biggest single
  amplifier or a 24h reputational dent. Mitigation = the soft venues
  upstream have already calibrated the pitch and produced positive
  early-adopter testimony to reference.

### 5. Lobsters — +1 day after HN

- **Timing:** day after HN, regardless of HN reception.
- **Format:** Lobsters submission with appropriate tags (`rust`,
  `webassembly`, `programming`). Link to GitHub. Same-day comment
  from the OP framing the project.
- **Why last:** Lobsters is a smaller, more curated audience that
  appreciates a project that has already been through r/rust and HN
  scrutiny without melting down. Late entry here often produces the
  longest-shelf-life conversation thread.
- **Risk:** low.

### Skipped (deliberately)

- **Twitter / X / Bluesky / Mastodon:** the operator's existing
  social-graph presence is the right surface for these; not a
  cargoless-team-coordinated venue.
- **dev.to / Medium / mass tech blogs:** SEO-noise venues; do not
  add signal proportional to the maintainer-time cost of submission.
- **YouTube / Twitch:** a launch demo video is a possible follow-up
  asset, NOT part of the v0 launch sequence. Recording + editing time
  is incompatible with launch-fortnight responsiveness.

---

## Spacing-and-pause rules

Between every two adjacent venues:

- **Minimum 24 hours.** Even if the prior venue's thread is dead,
  give the channel time to surface to anyone who didn't see it
  immediately.
- **Pause-on-fire rule.** If a launch-blocker bug surfaces at any
  venue, **halt the sequence until it lands a fix on `main` AND a
  github-release patch** (or the operator explicitly waives, e.g. for
  a documentation-only issue). The "responsiveness commitment" in
  CONTRIBUTING.md is load-bearing; missing it during the launch
  fortnight kills the trust pitch.
- **Pause-on-clearing-real-criticism rule.** If the response to a
  venue surfaces a legitimate critique we don't have an immediate
  answer to (e.g., "this benchmark methodology is wrong because X"),
  pause the next venue, draft + publish a written response addressing
  the critique, link the response from the next venue's submission.

## Launch-fortnight on-call rotation

For the **two weeks bracketing the launch sequence** (start: TWiR
submission day; end: 14 days after Lobsters submission), the
maintainer roster commits to:

- One named on-call maintainer per business day for issue triage,
  with the responsibility to acknowledge every new issue/PR within
  the CONTRIBUTING.md window.
- A second named human (the operator or a designated alternate) for
  cross-venue monitoring (HN ranking, Reddit thread, Discord
  pings) for the **48 hours after each venue submission**.
- A documented hand-off doc (likely a pinned note in the agent
  team's coordination channel) listing the active launch day's
  on-call name + the open issues being tracked.

The named roster is updated in this document before each venue
submission. (Roster names left as `<TBD>` until operator assigns —
this is the only document section that legitimately ships with TBD
markers; everything else must be filled in pre-launch.)

| Window | Primary on-call | Secondary (cross-venue monitor) |
|---|---|---|
| TWiR submission day (Wed) | `<TBD>` | `<TBD>` |
| r/rust submission day (Thu/Fri) | `<TBD>` | `<TBD>` |
| Leptos Discord submission day | `<TBD>` | `<TBD>` |
| HN submission day | `<TBD>` | `<TBD>` |
| Lobsters submission day | `<TBD>` | `<TBD>` |
| First-week follow-up | `<TBD>` | n/a |
| Second-week follow-up | `<TBD>` | n/a |

---

## After the launch fortnight

The launch sequence ends; the responsiveness commitment in
CONTRIBUTING.md transitions from the launch-fortnight cadence
(48h acknowledgement) to the sustainable cadence (one week
acknowledgement). The maintainer roster does a post-launch retro:
what worked, what surprised, what to do differently for the v0.1
launch.

This document is updated with the retro outcomes and frozen as the
historical record of the v0 launch. The v0.1 launch sequence reuses
the same shape (TWiR → r/rust → Leptos Discord → HN coin-flip →
Lobsters) but with the venue-specific lessons folded in.
