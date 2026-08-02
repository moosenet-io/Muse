# S130-K — Maestro: the Chromecast receiver, its hosting, and the cast auth handshake

plane_project: MUSE
module: Maestro
prefix: MCST
spec_id: S130-K-maestro-cast-receiver

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Maestro v0.1 (Cast surface; receiver app is a new TypeScript subtree in `moosenet/Muse`)
- **Estimated total:** ~54h (≈48h agent + ≈6h operator, of which the App ID registration is long-lead
  and asynchronous)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1, 2, 4, 5, 6, 7. **Clause 3 (context bus) is inherited, not
  deferred**: a cast session reports playback state back through the same Maestro → Muse event path
  every other session uses (spec D MDLV-08), so "what is being watched right now" is true of the
  living-room TV exactly as it is of the browser. **Clause 1 carries the epic §8.6 media-plane
  carve-out and widens it by one notch, deliberately and visibly:** a Cast device is the one client
  in the fleet that holds neither a Terminus cookie nor a bearer, so it authenticates with spec D's
  signed, session-scoped, expiring stream URLs (epic §8.7, spec D's MDLV-04) and nothing else. §4
  below specifies that handshake precisely — what K consumes from D unchanged, and the one
  route-level addition K genuinely needs — because it is what decides whether casting works or dies
  quietly at minute 40.
- **Context:** This spec is the **K** child of `S130-maestro-epic.md`. It was **split out of spec G**,
  and the split is not cosmetic. Cast support is a **different technology stack** (a TypeScript
  application that runs on the Cast device, not in our shell) sitting behind a **long-lead external
  dependency** (a Google-registered Cast Application ID, an asynchronous third-party process with a
  fee and a propagation delay). Leaving it embedded inside G meant G's Phase 2 could not honestly
  close until an external registration completed — a silent stall on the spec that delivers the
  Constellation's first video player. Pulling it out lets G finish on its own evidence and lets this
  spec absorb the waiting.

  It is also the spec where two of the epic's decisions collide and must be reconciled in code rather
  than prose: **plex mode serves no bytes** (§8.6), so there is a large and non-obvious class of
  content that simply cannot be cast; and **the media plane does not traverse Terminus** (§8.6/§8.7),
  so the receiver's only credential is a signed URL whose expiry it must survive without any path
  back to the control plane. §3 and §4 are the substance of this document; the items implement them.

---

## 1. What a Cast receiver actually is, and why that makes this a different spec

Casting is not a protocol for "send this URL to a TV". It is a small distributed system with three
participants, and every design decision below follows from the shape of it:

| Participant | What it is | Where it runs | What credentials it holds |
|---|---|---|---|
| **Sender** | The Cast Web Sender SDK inside constellation-web | The operator's browser | The shell cookie session |
| **Receiver** | A **web application we write and Google hosts nothing of** — HTML + TypeScript on top of Google's CAF receiver framework | **On the Chromecast itself** | **Nothing.** No cookie, no bearer, no shared secret |
| **Media plane** | Maestro's byte-serving routes | The Maestro host | n/a — it authenticates the request |

The receiver is launched **by App ID**. The sender asks the device to start application
`<APP_ID>`; the device asks Google's Cast infrastructure what URL that ID maps to; the device then
**fetches that URL over HTTPS and runs the page**. Three consequences that shape this entire spec:

1. **The receiver URL is registered with Google, not configured by us at runtime.** It is a fixed
   string decided at registration time. Changing it is a console operation with propagation delay,
   not a deploy.
2. **The Cast device fetches that URL itself, from the LAN.** It is not a browser we control, it has
   no proxy configuration, and it trusts only publicly-rooted certificate authorities. A private CA
   is not an option. This is the whole of §3.
3. **The receiver is an ordinary web page with an extraordinary network position.** It can fetch, it
   can run MSE, it can be given messages by the sender over a custom namespace. What it cannot do is
   present any credential it was not handed, or reach a control plane that expects a cookie.

**Why this is a genuine sovereignty wrinkle rather than a chore.** Everything else in the
Constellation is reachable because the client is inside the LAN and holds a fleet credential. The
Cast device is inside the LAN and holds nothing, and the thing that tells it what to run is a Google
service. We cannot make casting fully sovereign — the App-ID→URL resolution is Google's, permanently,
and there is no self-hosted substitute. What we *can* do, and what §3 recommends, is confine the
external dependency to **exactly that one lookup**: the receiver code is ours, served from our
hardware, and no media byte and no piece of library metadata ever leaves the LAN. That is the honest
sovereignty position, and it should be stated plainly rather than either ignored or overstated.

### 1b. Licence posture — stated first, because the temptation is real

**Jellyfin's Cast receiver is GPL-2.0. Do not read it, do not copy from it, do not port it.** The
fleet is MIT with public mirrors and epic §7.2 forbids GPL code entering any Constellation repo. This
is not a formality: a receiver is a small enough application that a "just look at how they handled
the LOAD interceptor" glance turns into derived structure very easily, and the mirrors are public.

The receiver is built on **Google's own CAF (Cast Application Framework) receiver SDK and its
published templates and documentation**, which is what that SDK exists for, plus original code. Every
non-obvious behaviour is implemented from Google's documentation with the documentation section cited
in a code comment, so a reviewer can check provenance in one hop. If a Jellyfin behaviour is
genuinely worth having, it is re-derived from the Cast documentation, and the item says so.

Note the asymmetry worth being precise about: the CAF SDK itself is **loaded by the Cast device from
Google's CDN at runtime** and is never vendored into our repo, so it never enters the tree at all.
Our repo contains only our own MIT TypeScript plus a `<script>` tag. That is a cleaner licence story
than bundling, and it is also simply how the CAF SDK is required to be loaded.

---

## 2. Relationship to spec G — what this spec takes over, cleanly

Spec G (`S130-G-maestro-player-gui.md`) still contains two Cast items, written before the split:

| Spec G item | Status under this spec |
|---|---|
| **MPLY-10** — Chromecast sender support, feature-flagged default OFF | **SUPERSEDED by MCST-08** (sender integration) and **MCST-09** (handoff). Its design intent is carried forward verbatim where it was right — the App ID comes from deployment configuration and never from source, the Cast transport is one more `useTransport` implementation and adds zero branching to `PlayerControls`, and an unreachable-from-receiver URL disables the entry with a reason rather than failing on the device. |
| **MPLY-11** — Register a Cast App ID and publish the receiver (operator action) | **SUPERSEDED by MCST-01** (registration) and **MCST-02** (hosting + TLS + URL registration). Its step 2 — "the Styled Media Receiver is sufficient for the first pass" — is **reversed** by §3 below, with reasons. |

**These two items are NOT to be ingested as Plane items from spec G.** If they were already created,
close them as superseded with a comment naming the MCST item that replaces them. Duplicated Plane
items for one piece of work is precisely the tracking failure the mandatory-pipeline rule exists to
stop, and it is easiest to create at a spec boundary like this one.

**A pre-flight action in §7 amends spec G** to point MPLY-10/11 at this document rather than leaving
two specs that both appear to own casting. A reader arriving at G six months from now must not have
to infer the split.

**What G keeps, and what this spec must not touch.** G owns the player surface: the `<video>` element,
the control bar, the target menu, the session lifecycle, the `/why` card. This spec adds **one target
kind** to G's existing `useTransport` abstraction and **one receiver application**; it does not
restyle a control, does not add a panel, and does not change how a local session works. If an MCST
item finds itself editing `PlayerControls.tsx`, MPLY-09's abstraction was wrong and the fix belongs
there — that is G's own stated rule and it holds across the split.

---

## 3. Receiver hosting — the real design problem, and the recommendation

The receiver must be **fetched over HTTPS by a Cast device on the LAN, from a URL registered with
Google**. That single sentence contains every constraint. Four options were considered.

### 3a. The options

**Option 1 — Google's Default Media Receiver (App ID `CC1AD845`, no registration, no hosting).**
Zero cost, zero infrastructure, available today. It plays a URL you hand it and reports standard
media status. **It cannot run any code of ours**, which means: no custom message namespace, no token
refresh, no way to survive an expiring signed URL, and no way to report anything richer than the
standard media status. It is not a candidate for the product — but it *is* the right tool for an
early hardware spike, and §5 uses it for exactly that.

**Option 2 — Google's Styled Media Receiver (registered App ID, we host only a CSS file).**
Registration required; hosting is one publicly-reachable HTTPS CSS file. Still Google's JavaScript,
so it inherits every limitation of Option 1 plus a registration. It buys branding and nothing else.
**MPLY-11's "the Styled Media Receiver is sufficient for the first pass" is wrong**, and the reason
is §4: the auth handshake this design needs is custom-namespace code running on the receiver, and a
styled receiver cannot run code. Choosing it would mean discovering at integration time that casting
cannot be authenticated at all — the most expensive possible moment.

**Option 3 — Custom receiver, bundle hosted on a public static host** (the public GitHub mirror's
Pages, or any static host). No certificate work, no DNS work, works immediately, and the *content*
still never leaves the LAN — only the application shell is public, and it is MIT source we already
mirror. The costs are real though: an external service sits in the boot path of the living-room TV,
so an outage at a third party stops playback on our own hardware with our own files; and the receiver
page origin differs from Maestro's media origin, which drags in CORS and — the sharp one — **mixed
content**. A page loaded over HTTPS cannot fetch media over HTTP, so this option forces TLS onto
Maestro's media plane anyway. It solves the easy half of the problem and leaves the hard half.

**Option 4 — Custom receiver, served by Maestro itself over TLS on an internal hostname with a
publicly-valid certificate.** The receiver bundle is a route on Maestro (`GET /cast/`), Maestro
terminates TLS with a certificate issued by a public CA for a hostname the operator owns, and DNS for
that hostname resolves to the LAN address (split-horizon, or simply a public A record pointing at a
private address — which is legal, common, and exposes nothing). The certificate is obtained by
**DNS-01** challenge, so **no inbound port is ever opened** and nothing is published. The Cast device
resolves the name, connects to a LAN address, and gets a certificate it trusts because the CA is
public.

### 3b. The recommendation: Option 4, and the reason is same-origin

**Recommendation: Option 4 — a custom receiver served by Maestro itself, over TLS, on an internal
hostname with a publicly-valid DNS-01 certificate.** Not merely because it is the sovereign choice,
but because it is the one that **deletes a whole class of failure** rather than managing it:

1. **Same origin for the receiver page and the media.** The receiver is served from `/cast/` on the
   same host, port and scheme as `/playback/{id}/stream`. That means **no CORS configuration at all**
   — no preflight, no `Access-Control-Expose-Headers: Content-Range` to forget, no discovering at
   integration time that `Range` was not in the allowed-headers list. Cross-origin media on a Cast
   receiver is a well-known source of "works in the browser, fails on the device", and the cheapest
   way to be right is to not be cross-origin.
2. **Mixed content is structurally impossible.** An HTTPS receiver page fetching HTTPS media on the
   same origin cannot be blocked. Options 1–3 all end up requiring TLS on the media plane regardless
   (a Cast receiver page is always HTTPS), so Option 4 does not add the TLS requirement — it just
   stops us paying for it twice.
3. **No extra process in the media path.** Maestro terminates its own TLS rather than sitting behind
   a reverse proxy. Epic §8.6 kept media off the Terminus gateway specifically so playback uptime is
   not coupled to another service's restarts; inserting a proxy instead would re-introduce the same
   coupling with a different name. One process, one failure domain, consistent with §2 of the epic.
4. **The external dependency shrinks to one lookup.** Google resolves App ID → URL. Everything after
   that is our hardware. That is the smallest the dependency can be made, and it is worth saying so
   in the README because it is the honest answer to "is casting sovereign?".
5. **The operator cost is a one-time ops action, not ongoing work.** A DNS-01 wildcard certificate on
   a domain already owned, renewed by the existing automation, plus one A record. No port forward, no
   public exposure, no per-deploy step.

**What Option 4 costs, stated honestly.** It needs a domain the operator controls and a DNS provider
the ACME client can use for DNS-01; it makes Maestro a TLS terminator (a new responsibility, though a
small one with `rustls`); and a certificate that fails to renew breaks casting silently until someone
notices — so MCST-03 carries an expiry check in Maestro's health output rather than leaving it to be
discovered by a TV. **Option 3 remains a documented emergency fallback** — if the certificate story
stalls, the bundle can be published to a static host and the App ID repointed, at the cost of CORS
configuration and an external dependency. Record it in the README as the fallback; do not build it.

**Also settled by choosing Option 4: where the bundle lives and how it deploys.** The receiver is a
`cast-receiver/` subtree in `moosenet/Muse` with its own `package.json` and Vite build, built to
`cast-receiver/dist/` which is **committed to git** and embedded into the `maestro` binary with
`include_dir!`. This follows the constellation-web precedent exactly, including its hard-won lesson:
**there is no npm step in the OCI publish, so a receiver change that does not rebuild and commit
`dist/` deploys nothing.** That failure mode already cost the fleet a debugging cycle once
(TERM #550) and it will present here as "the TV is running the old receiver", which is materially
harder to spot.

---

## 4. The auth handshake — the piece that makes or breaks casting

This section is the reason the spec exists. Read it before implementing any item.

### 4a. What spec D already owns — consume it, do not rebuild it

**Spec D's MDLV-04 is "Signed, session-scoped, expiring stream URLs", and it is complete.** An earlier
draft of this spec reported a gap here. **That finding was false and is withdrawn** — it was made
against a stale view of `S130-D-maestro-delivery.md` while that file was being rewritten. The
correction is recorded rather than quietly deleted, because the failure mode is worth naming: a
reviewer whose view of a file is scope-limited or out of date produces an alarming "you did not do X"
that means only "X was not visible to me". Re-read before escalating.

**This spec therefore consumes, and must not re-implement, the following. All of it is D's:**

| Mechanism | Owner | What K does with it |
|---|---|---|
| `sign()` / `verify()` — HMAC-SHA256, `canonical = "v1\|{session_id}\|{exp_unix}"`, base64url | `src/maestro/auth/signing.rs` (MDLV-04) | Calls it. Adds no second signer, no second key, no second canonical format |
| URL shape `{MAESTRO_PUBLIC_BASE_URL}/playback/{id}/stream?exp=…&sig=…` | MDLV-04 | The receiver plays what it is handed. `MAESTRO_PUBLIC_BASE_URL` is also the answer to "is this URL reachable from the receiver's network position" |
| TTL `MAESTRO_STREAM_URL_TTL_SECS`, **default 6h** | MDLV-04 | **D's 6h wins — see §4b.** K sets no TTL of its own |
| Constant-time compare, `MAESTRO_CLOCK_SKEW_SECS` tolerance, undetailed `403` | MDLV-04 | Inherited unchanged; the receiver classifies on the status, never on a detail D deliberately withholds |
| Key rotation via `MAESTRO_STREAM_SIGNING_KEY` + `_PREVIOUS` | MDLV-04 | Inherited. A rotation drains live cast sessions exactly as it drains browser ones |
| **Fail-closed:** unset signing key ⇒ **the media plane refuses to start** | MDLV-04 | Inherited verbatim. Cast is not a special case that softens this, and no item in this spec may degrade it to "cast unavailable, carry on" |
| No client-IP binding — considered and rejected, precisely because the Cast device fetches from a different IP than the sender | MDLV-04 | Nothing to do. Do not "fix" it |
| `403` (bad/expired/missing signature) vs `410 Gone` (valid signature, dead session) vs `404` | MDLV-04 / §2 | This distinction is the receiver's entire error model — §4c |
| URL renewal: `HeartbeatAck { interval_secs, stream_url, expires_at }` refreshes a near-expiry URL under 25% remaining | MDLV-08 | **The mechanism K needs — but it is on the control plane. That is the delta. See §4c.** |

**One consequence worth stating loudly, because it is the opposite of what the earlier draft assumed:
there is no second token type in this design.** No refresh token, no exchange endpoint, no
scope-separated canonical string. A design with two tokens was drafted here and is **dropped** — with
D's 6h TTL and D's heartbeat-ack renewal, it earned nothing but a second credential to keep valid.
The receiver holds exactly one credential: the signed URL. §9 records the drop so it is not
re-proposed.

### 4b. TTL reconciliation — D's 6h wins, explicitly

An earlier draft of this spec specified a 15-minute stream-token TTL on the reasoning that a
short-lived URL is worthless by the time it leaks. **D's 6h default wins, and D's reasoning is
better**: *"A token that expires mid-film is a bug, not security; the session lifecycle is the real
bound and it is tighter."* That is correct, and it is correct **specifically because of Cast**. The
containment argument does not depend on the TTL anyway — MDLV-04 step 2 is that the token is scoped
to one session and one item and dies with the session, and MDLV-08's reaper guarantees the session
dies. Shortening the TTL would have bought a marginal reduction in leak window at the cost of making
mid-playback renewal load-bearing on the one client that can least afford it.

**Practical consequence, and it changes where the risk actually is.** At a 6h TTL, a cast session
started fresh and watched straight through **never renews at all** — a film is 2–3h, the token is
valid for 6. Token expiry is therefore *not* the likely defect in cast playback. §4c's actual problem
is a different and more mundane one, and the earlier draft's emphasis was misplaced.

### 4c. The one genuine delta: the media plane has no heartbeat, and the receiver needs one

**Spec D's heartbeat is control plane.** Its §2 surface table places
`POST /playback/{id}/heartbeat` under *"CONTROL PLANE (through Terminus `proxy_maestro`,
bearer-authenticated)"*, and MDLV-08 is what returns both the server-directed `interval_secs` and the
renewed `stream_url`. That is exactly right for the browser, which holds the shell cookie and reaches
Maestro through the gateway.

**A Cast receiver holds no cookie and no bearer.** It therefore cannot call the heartbeat — and this
is not merely a missed renewal. Two consequences, and the second is the serious one:

1. **It never receives a renewed URL.** Low impact at a 6h TTL (§4b), but it means a genuinely long
   session — a film paused overnight and resumed — has no path back.
2. **It gets reaped in ninety seconds the moment it pauses.** This is the real defect. MDLV-08 closes
   any session with no heartbeat *and no bytes* for `MAESTRO_SESSION_IDLE_TIMEOUT_SECS` (default 90).
   A playing receiver survives on byte-pull alone — MDLV-08 step 5 says so explicitly and correctly.
   **A paused one pulls no bytes.** So: user pauses the film to make tea, comes back in two minutes,
   and the session is `410 Gone`. That is a product-breaking behaviour that no test of the happy path
   will ever surface, and it is the single most important thing this spec found.

**The delta on spec D, stated precisely as an addition rather than a correction.** D's heartbeat is
correct for the client D was designing for; it simply has no media-plane-authenticated form, and
nothing in D claims otherwise. What K needs is one route-layer change:

> **`POST /playback/{id}/heartbeat` must also be accepted on the media plane, authenticated by the
> signed URL's `?exp=&sig=` instead of the bearer** — same handler, same `HeartbeatRequest`, same
> `HeartbeatAck` (including its optional renewed `stream_url`), same `410` on a reaped session. D
> already has a verification `route_layer` on the media plane (MDLV-04 step 4); this places heartbeat
> behind it as well.

That is the whole of it. **No new credential, no new token type, no new endpoint, no change to D's
canonical string, TTL, key handling, or error mapping.** The receiver uses the one credential it was
handed — the signed URL — for both bytes and liveness, and receives its renewed URL through D's own
existing ack. MCST-04 implements this delta and nothing more.

**The receiver's resulting behaviour, which is the rest of K's share:**

1. **Heartbeat on the cadence the ack dictates** (`interval_secs`), not a cadence the receiver picks.
   Server-directed cadence is D's design and it must not be second-guessed on the device we can least
   easily change.
2. **Heartbeat while paused, not only while playing.** This is the inverse of MPLY-08's browser rule
   (which correctly stops heartbeating when paused, because the browser's session is torn down on exit
   anyway). For the receiver, the pause heartbeat is the *only* thing keeping the session alive.
3. **Apply a renewed `stream_url` whenever an ack carries one.** The receiver does not compute when to
   renew — it does not know the TTL and should not. If `stream_url` is present, adopt it.
4. **Applying a renewed URL to media already in flight is the genuinely hard part**, and it is where
   the HLS trap below lives. This, not token arithmetic, is MCST-05's real work.
5. **Error model, straight from D's table:** `403` ⇒ the signature is bad or expired and there is no
   detail coming; try one renewal via heartbeat, then fail terminally. `410` ⇒ the session is gone;
   **stop, do not retry** — only the sender can open a new one. `404` ⇒ unknown session, terminal. A
   receiver that retries a `410` sits there all night on a television nobody is looking at.

**The HLS sharp edge, which survives every simplification above.** A renewed URL is useless to a
transcode session unless the *playlist* is re-rendered with it: segment URIs each carry their own
signature (spec E §1c stamps them at render time). For a **live or EVENT** playlist the player
re-fetches the media playlist anyway, so renewed signatures arrive naturally. For a **VOD** playlist —
one that has emitted `#EXT-X-ENDLIST` — **the player never re-fetches it**, so its segment signatures
must outlive the remaining playback or it dies with no reload to save it. Two rules, both acceptance
criteria:

1. A transcode session serves an **EVENT** playlist while the encoder still runs, so renewal rides the
   reload the player already performs.
2. On a **VOD/complete** playlist, the receiver must **explicitly re-fetch the media playlist** with
   the renewed URL and hand the new URIs to the player. A receiver that adopts a renewed URL and never
   re-renders the playlist has renewed nothing.

**Why not sender-mediated renewal (recorded so it is not re-proposed).** The sender *can* reach the
control plane and could push a renewed URL over the custom namespace. It is simpler, and it is wrong
for the actual use case: the point of casting is to start a film and close the laptop. Anything that
requires the sender to stay connected turns "close the tab" into "the film stops", which fails late
and looks like a bug in the media. Worse, it would not solve §4c.2 at all — the pause-reap needs a
heartbeat from something that is still there, and the sender is precisely the thing that is not.

## 5. What is castable, and what is not — say it, do not let it be discovered

Epic §8.6 is unambiguous and its consequence here is large: **`plex` mode is control and observe only.
No bytes flow through Maestro, and there is nothing for a receiver to load.** Our receiver can
therefore only play content that Maestro's `native` backend is serving.

This produces a genuinely confusing situation for a user, and the failure mode if it is left implicit
is the worst kind: the Cast button is there, the device is there, and nothing happens — or worse, it
half-works for some items and not others with no visible pattern. So it is specified rather than
inferred:

| Situation | Castable to the Maestro receiver? | What the UI must say |
|---|---|---|
| Backend `native`, plan is direct-play or remux | **Yes** | Nothing — it just works |
| Backend `native`, plan is transcode | **Yes, once spec E lands.** Before E, no | "Casting this item needs the transcode path" |
| Backend `plex` | **No — Plex serves the bytes, Maestro never sees them** | "Plex-backed items can't be cast by Maestro. You can still send this to a Plex client from the target menu." |
| Item has no playable file | No | The existing MPLY-02 inert-tile treatment |
| Plan is `Undecidable` (spec C) | No | Spec C's own reason, verbatim |
| HDR item to an SDR-only device | Out of scope through spec E (epic §8.3) | The plan's reason, verbatim — never a fabricated one |

**The distinction that must survive into the UI: "cast" is two different things.** A Chromecast that
is running the *Plex* receiver is a controllable Plex client, reachable today through the existing
`CastController`/`PlexControlClient` seam and offered in MPLY-09's target menu. That is Plex casting
to Plex, and it works. Our receiver is a *different application on the same hardware*. Presenting
both as an undifferentiated "Cast" entry would be actively misleading — they have different
capabilities, different content coverage, and different failure modes. MCST-07 keeps them distinct
and labelled.

---

## 6. Verification — honest about what headless CI cannot do

**Headless CI cannot cast.** There is no emulator, the Cast transport needs real hardware on a real
network, and the receiver runs on a device we cannot instrument. Pretending otherwise is how "cast
support merged" becomes a claim nobody checked. So verification is explicitly four layers, and the
top layer is a human with a TV.

1. **Pure unit tests (CI, full confidence).** **The signer's own tests are spec D's** (MDLV-04) and
   are not duplicated here. K's units are: the media-plane heartbeat's authentication and its
   ack-parity with the control-plane form (MCST-04), and the receiver's pure logic — renewal adoption,
   the VOD-vs-EVENT playlist-reload decision, and error classification over D's `403`/`410`/`404`
   vocabulary. This layer is where most of the *correctness* lives and it is cheap.
2. **Receiver DOM tests (CI, good confidence).** The receiver is an ordinary web page. It is loaded
   in jsdom/headless with a **stubbed `cast.framework`** — a hand-written test double implementing the
   `CastReceiverContext`, `PlayerManager` and message-interceptor surfaces the app uses — and asserted
   on behaviour: a heartbeat ack carrying a renewed `stream_url` is adopted; a `410` stops rather than
   loops; a VOD playlist triggers an explicit reload.
3. **Live loopback harness (<host>, real confidence in the wiring).** The same stubbed-framework
   receiver, pointed at a **live Maestro**, driven headless. This proves the parts that a stub cannot:
   the token actually verifies, the refresh endpoint actually answers, the playlist actually re-renders
   with new tokens, a `410` on a closed session actually arrives. It proves everything except the Cast
   transport and the device's decoder. Runs on the existing Playwright harness host.
4. **Real hardware, operator, recorded (MCST-11).** A fixed matrix — device model × container × tier ×
   **token expiry during playback** — run by the operator against the household's actual devices and
   written down as a table in the Plane item **and** committed as `docs/cast-verification.md` in the
   repo, so the next person knows which devices were ever proven and when. Re-run on any receiver
   change that touches load, refresh or error handling.

**The reporting rule, and it is a hard one.** *"Casting works"* is claimable **only** with layer 4's
table filled in for at least one device. Layers 1–3 justify *"the cast path is implemented and its
wiring is verified; end-to-end playback on hardware is pending MCST-11."* Spec G's MPLY-10 already
carried this discipline ("the PR states plainly that end-to-end casting is unverified") and it is
inherited, not softened. An item that reports success from a headless run that never reached a device
is a false pass and will be treated as one.

**The one thing layer 4 must deliberately test that no one thinks to test: pause the film and walk
away for five minutes.** Per §4c.2, a paused receiver pulls no bytes, and MDLV-08 reaps a session with
no bytes and no heartbeat after ninety seconds. Every natural test — start it, watch a bit, stop it —
misses this completely, and the symptom is a film that dies while nobody is in the room, which is
maximally hard to attribute afterwards. It is a five-minute test that catches the single most likely
defect in this spec.

A secondary run lowers `MAESTRO_STREAM_URL_TTL_SECS` to exercise D's renewal path on a device, since
at the 6h default (§4b) renewal never fires within a normal viewing and would otherwise ship
unexercised on the receiver.

---

## 7. Pre-flight

- **Repository:** `moosenet/Muse` — Maestro's modules under `src/maestro/`, the receiver under a new
  `cast-receiver/` subtree. Plus **one** cross-repo change: the sender lives in `moosenet/Terminus`
  under `constellation-web/` (MCST-08/09), which is a separate PR in a separate repo per the epic's
  ownership split. **No new repo.**
- **Spec D must have landed** its session model, byte routes, MDLV-04's signing, and MDLV-08's
  heartbeat/reaper. **Read §4a first** — D owns the signer, the TTL, the key handling and the error
  mapping, and this spec consumes all of it unchanged.
- **One dependency delta on spec D, stated precisely (§4c).** D's heartbeat is control-plane and
  bearer-authenticated, which is correct for the browser and simply does not reach a Cast receiver.
  K needs `POST /playback/{id}/heartbeat` **additionally accepted on the media plane behind MDLV-04's
  existing verification layer** — same handler, same `HeartbeatAck`, same `410`. That is an addition
  to a route table, not a correction of an omission, and MCST-04 implements it. If spec D would rather
  own it, it is a four-line change there and this spec consumes it instead; either way there must be
  exactly one heartbeat handler.
- **Spec E is required for casting transcoded content**, not for casting at all. Direct-play and
  remux items are castable on D alone. Do not sequence this spec behind E.
- **Amend the sibling specs so ownership is unambiguous** (a documentation change, done once, before
  ingest):
  - `S130-G-maestro-player-gui.md` — mark MPLY-10 and MPLY-11 SUPERSEDED by this spec, naming the
    MCST items, and correct MPLY-11's "Styled Media Receiver is sufficient" per §3a.
  - `S130-maestro-epic.md` §8.4 — the decision text still says the App ID is "deferred to spec G";
    §11 already reverses that. Reconcile the two so they do not contradict, and point both at K.
  - `S130-maestro-epic.md` §11 — the App-ID pre-flight says registration gates spec E's
    CMAF-on-real-hardware verification. It does not: MTRX-03 can run today on Google's Default Media
    Receiver (MCST-01). The App ID gates **K**, not E.
  - **No amendment to spec D or spec E is required.** D owns the signer and E correctly consumes it.
- **Operator prerequisites, all long-lead or ops-only:**
  - A Google Cast developer account and a registered Application ID (MCST-01) — **asynchronous, has a
    propagation delay, start it on day one**.
  - A domain the operator controls, with a DNS provider usable for ACME **DNS-01**, and a certificate
    issued for the internal hostname (MCST-02). No inbound port is opened.
  - Secrets in <secret-manager>, materialised at runtime, never authored into a file by hand:
    `MAESTRO_CAST_APP_ID`, `MAESTRO_TLS_CERT_PATH` / `MAESTRO_TLS_KEY_PATH` (paths, not material).
    `MAESTRO_STREAM_SIGNING_KEY` (+ `_PREVIOUS`) is **spec D's** and is already a D pre-flight —
    listed here only because cast is unusable without it, not because K provisions it.
  - The Cast devices registered as test devices in the console (MCST-01) — required before a
    non-published receiver will load on them at all, and a classic first-day blocker.
- **Baselines to record before starting:** `cargo test` green on Muse main with its count; and for
  the Terminus-side items, `npm run typecheck`, `npm run test` and the current
  `npm run lint:adherence` warning count.
- **Prefix:** `MCST` — confirm free with `plane_prefix_check`, claim with `plane_prefix_register`,
  then `plane_prefix_promote` for the durable baseline entry.
- **Build/test host:** `ffmpeg`/`ffprobe` are **not** on the dev box (epic §11). Anything exercising a
  real stream runs through the compiler tool on a host that has them.

---

## Items

### MCST-01: Register the Google Cast Application ID and the household test devices
- **Priority:** Critical
- **Labels:** maestro, cast, operator, long-lead
- **Agent:** <operator>
- **Estimate:** 1h (plus asynchronous registration and propagation delay)
- **Type:** human-action
- **Description:** **Start this on day one of the sprint, before any code.** A Cast receiver is
  launched by a registered Application ID, and registration is a third-party process with a one-time
  developer fee (~$5), a manual review step, and a propagation delay measured in hours rather than
  minutes. Nothing about it is hard; everything about it is *slow*, and discovering the lead time
  mid-sprint costs a slip for no engineering reason. Epic §11 already reversed the earlier decision to
  defer this — this item is that reversal executed.

  **What it gates, precisely, because the epic's §11 wording overstates one of them:**
  - **This spec, entirely.** MCST-03 onward cannot be verified on a device without an App ID, and
    MCST-05's receiver cannot be launched at all.
  - **Spec E's MTRX-03 CMAF-on-real-hardware spike — but only for the *representative* verification.**
    MTRX-03 asks whether the household's devices play fMP4/CMAF-in-HLS, and that question can be
    answered **today, with no registration**, using Google's **Default Media Receiver**
    (`CC1AD845`), which needs no App ID and no hosting. **MTRX-03 should use it and should not wait
    for this item.** What the Default Media Receiver cannot tell us is whether *our* receiver handles
    those containers under *our* auth handshake — that is MCST-11's job. Record this distinction in
    the item so MTRX-03 is not parked behind a registration it does not need.

- **Steps:**
  1. Register a Google Cast developer account and pay the one-time fee.
  2. Create a **Custom Receiver** application (not Default, not Styled — see §3a for why). Its URL is
     set in MCST-02 once the hostname exists; if the console requires a URL at creation, use a
     placeholder and update it there.
  3. **Register every Cast-capable device in the household as a test device** (serial number from the
     device's settings). An unpublished receiver will not load on an unregistered device, and this is
     the single most common first-day blocker — do it now, not when the first cast fails.
  4. Record the App ID in **<secret-manager>** as `MAESTRO_CAST_APP_ID`, materialised into Maestro's runtime
     env. **Never committed to source, never written into a hand-authored `.env`** (S1/S7).
  5. Note the propagation delay observed between saving a receiver URL and a device picking it up —
     write the real number into the Plane item, because every later debugging session will want it.
  6. Report the App ID's existence (not its value) and the device list back so MCST-05 and MCST-11 can
     proceed.

### MCST-02: Receiver hosting — internal hostname, DNS-01 certificate, and the registered URL
- **Priority:** Critical
- **Labels:** maestro, cast, operator, tls, infra
- **Agent:** <operator>
- **Estimate:** 2h
- **Type:** human-action
- **Blocked by:** MCST-01
- **Description:** Stand up the hosting position §3b recommends: an internal hostname on a domain the
  operator controls, resolving to the Maestro host's LAN address, with a **publicly-valid certificate
  obtained by DNS-01** so the Cast device — which trusts only public CAs and cannot be given ours —
  will complete the TLS handshake.

  **No inbound port is opened and nothing is published.** DNS-01 proves domain control through a DNS
  TXT record, so the ACME server never connects to us. The A record may point at a private address;
  that is legal, ordinary, and reveals only that a name exists.

  This is an ops action touching no tracked code, which is exactly why it is a human-action item —
  but MCST-03 cannot be verified without it, so it is sequenced first rather than treated as a
  post-hoc fix. That ordering is the lesson of TERM #549, applied before it bites rather than after.

- **Steps:**
  1. Choose an internal hostname on a domain the operator already controls. Record it **in <secret-manager>
     and the ops config only** — it is a real hostname and therefore never appears in this repo, in a
     spec, or in a commit message (S1).
  2. Add the A record resolving it to the Maestro host's LAN address. Confirm resolution from a device
     on the household network, not only from the dev box.
  3. Issue a certificate via **DNS-01** with the existing ACME automation. Confirm the renewal hook is
     wired — a silent renewal failure presents months later as "casting stopped working" with no
     other symptom.
  4. Materialise `MAESTRO_TLS_CERT_PATH` and `MAESTRO_TLS_KEY_PATH` into Maestro's runtime env
     (paths, never key material) alongside the MCST-01 secret. Provision them **in one action** with
     `MAESTRO_STREAM_SIGNING_KEY` — splitting credential provisioning across separate ops passes is
     precisely how TERM #549 happened.
  5. Set the receiver URL in the Cast console to `https://{internal-hostname}/cast/` and record the
     observed propagation delay.
  6. Verify from a household device (not the dev box) that the URL serves over HTTPS with a trusted
     chain, before MCST-03 is called done.

### MCST-03: Maestro TLS listener and the embedded receiver bundle route
- **Priority:** Critical
- **Labels:** maestro, http, tls, cast
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MCST-02 (for verification; the code can be written in parallel)
- **Description:** Make Maestro serve its media plane **and** the receiver bundle over TLS from one
  origin, so §3b's same-origin property is real rather than aspirational. Two deliverables: a `rustls`
  listener configured from the MCST-02 paths, and `GET /cast/*` serving the receiver bundle embedded
  in the binary.

  **Same origin is the whole point and must not be quietly given up.** If a later change moves the
  receiver behind a different host or port, CORS and mixed-content handling come back — so the route
  lives on the same router and the same listener as `/playback/{id}/stream`, and a test asserts they
  share an origin rather than trusting configuration.

  **TLS is additive, not a replacement.** The plain-HTTP listener stays for the browser path through
  `proxy_maestro` (the gateway reaches Maestro over the LAN and needs no TLS), so this item must not
  break any existing client. Config-gated per epic §7.4: no cert paths configured ⇒ no TLS listener,
  no `/cast/` route, and `BackendCapabilities` reports cast unavailable with that reason. Absent, not
  broken.

  ## FILES
  - `src/maestro/http/tls.rs` — new: rustls config load, listener, cert-expiry inspection
  - `src/maestro/http/cast_assets.rs` — new: the `include_dir!`-embedded bundle handler
  - `src/maestro/http/mod.rs` — route registration + dual-listener startup
  - `src/maestro/config.rs` — `MaestroConfig` gains the TLS paths and the cast gate
  - `cast-receiver/dist/**` — the committed bundle (produced by MCST-05; a placeholder index until then)
  - `Cargo.toml` — `rustls`/`tokio-rustls`/`rustls-pemfile` and `include_dir`
  - `README.md` — document the listener, the cast route, and the §3b hosting decision

  ## APPROACH
  1. Load cert + key from `MaestroConfig` paths through the normal config seam. **The paths come from
     config; the key material is never in config, never logged, and never in an error message** — a
     load failure reports the path and the error kind, not the contents.
  2. Both listeners serve the **same `axum` Router**. Do not build a second router for TLS: two
     routers is how a route silently exists on one origin and not the other, which would present as
     "casting works from the browser but not from the TV" and take a day to find.
  3. `GET /cast/` and `GET /cast/*path` serve from `include_dir!("cast-receiver/dist")`. Path
     resolution is by **lookup in the embedded map only** — there is no filesystem and therefore no
     traversal surface, which is the same structural argument MDLV-02 makes for the media path.
     Unknown path ⇒ `404`; `/cast/` ⇒ `index.html`. Correct `Content-Type` per extension.
  4. `Cache-Control: no-store` on `index.html` (so a receiver update is picked up on the next launch
     rather than whenever the device feels like it) and long-lived immutable caching on hashed asset
     filenames.
  5. **Certificate expiry in the health payload.** Parse `not_after` at load and expose days-remaining
     in Maestro's health output. A certificate that quietly fails to renew otherwise surfaces as a TV
     that stopped working, months later, with no other signal. Cheap here, expensive anywhere else.
  6. TLS unconfigured ⇒ startup logs one line saying cast is disabled and why, then proceeds normally.
     Never a hard failure: a Maestro that refuses to start because casting is unconfigured has traded
     a missing feature for a missing media server.

  ## TEST PLAN
  - `cargo test` — cast-asset handler: `/cast/` returns the index with `text/html`; a hashed asset
    returns its correct MIME; an unknown path returns `404`; a `..`-bearing path cannot escape the
    embedded map
  - `cargo test` — router identity: the TLS and plain listeners are constructed from the same router
    value, asserted structurally rather than by comparing route lists by hand
  - `cargo test` — config: absent cert paths ⇒ no TLS listener, no `/cast/` route, capability reports
    unavailable with a reason; malformed PEM ⇒ a startup error naming the path and **not** the contents
  - `cargo test` — cert expiry parsing from a fixture cert, including an already-expired one
  - Live (on a host with the real cert): `curl` the receiver URL over HTTPS and confirm a trusted chain
    and a `200` — and confirm the **same origin** serves a session's stream route
  - Verify no hardcoded IPs, hostnames or tokens in new/modified files

  ## EDGE CASES
  - Cert and key present but mismatched → a clear startup error naming both paths, never a listener
    that accepts connections and fails every handshake
  - Cert renewed on disk while Maestro runs → documented as requiring a restart (a reload watcher is a
    follow-up, not this item); the health expiry figure is what makes the restart schedulable
  - `cast-receiver/dist/` missing at compile time → `include_dir!` fails the build loudly, which is
    correct; the placeholder index exists so this never blocks MCST-03 landing before MCST-05
  - Both listeners on the same port → refuse at startup with an explicit message rather than a bind race

- **Acceptance criteria:**
  - [ ] Maestro serves TLS from the configured cert/key alongside its existing plain listener
  - [ ] `/cast/` serves the embedded receiver bundle from the **same origin** as the media routes, proven by test
  - [ ] With TLS unconfigured, no TLS listener and no `/cast/` route exist, and the capability says why — startup still succeeds
  - [ ] Key material never appears in a log, an error message, or a health payload
  - [ ] Certificate days-remaining appears in the health output
  - [ ] An unknown or traversal-shaped `/cast/` path cannot reach anything outside the embedded bundle
  - [ ] README documents the hosting decision (§3b) and names Option 3 as the recorded fallback
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-04: Media-plane heartbeat — the one delta on spec D
- **Priority:** Critical
- **Labels:** maestro, cast, session, auth
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** spec D (MDLV-04 signing, MDLV-08 heartbeat + reaper)
- **Description:** Implement §4c's single addition: accept `POST /playback/{id}/heartbeat` on the
  **media plane**, authenticated by the signed URL, so a Cast receiver — which holds no cookie and no
  bearer — can stay alive and receive its renewed URL.

  **Read §4a before starting.** Spec D owns the signer completely: HMAC-SHA256 over
  `v1|{session_id}|{exp_unix}`, a 6h TTL, clock-skew tolerance, constant-time compare, `_PREVIOUS`-key
  rotation, fail-closed on a missing key, and the `403`/`410`/`404` mapping. **None of that is
  re-implemented here, and no second token type is introduced.** An earlier draft of this spec
  reported a signer gap in D; that finding was false and is withdrawn. If this item finds itself
  writing a `sign()`, it has gone wrong — call D's.

  **What justifies the item is not renewal, it is the ninety-second reap.** MDLV-08 closes any session
  with no heartbeat and no bytes for `MAESTRO_SESSION_IDLE_TIMEOUT_SECS` (default 90). A *playing*
  receiver survives on byte-pull alone, which MDLV-08 step 5 anticipates correctly. **A paused one
  pulls no bytes and cannot heartbeat**, so it dies in ninety seconds — the user pauses to make tea and
  comes back to `410 Gone`. Renewal is the smaller half of this item; not being reaped mid-pause is the
  reason it exists.

  ## FILES
  - `src/maestro/http/mod.rs` — place the existing heartbeat handler behind MDLV-04's media-plane
    verification layer in addition to its control-plane registration
  - `src/maestro/http/heartbeat.rs` — accept either authentication, with no divergence in behaviour
  - `README.md` — document the dual-plane heartbeat and why the receiver needs it

  ## APPROACH
  1. **One handler, two authentications, zero behavioural divergence.** Do not fork the handler. The
     control-plane registration keeps its bearer; the media-plane registration sits behind MDLV-04's
     existing `route_layer`. Two handlers is how the ack's renewal logic ends up correct on one path
     and stale on the other, and the divergence would be invisible until a device hit it.
  2. The response is D's `HeartbeatAck` unchanged — `interval_secs`, the optional renewed `stream_url`,
     `expires_at`. **This item adds no field and changes no semantics.** The renewal rule
     (issue a fresh URL under 25% of TTL remaining) is MDLV-08's and stays there.
  3. `410 Gone` on a reaped session is inherited verbatim, and it is load-bearing for the receiver:
     it is the difference between "renew and carry on" and "stop, the sender must re-open".
  4. **A paused session must be kept alive by heartbeat alone.** Assert it: a session receiving only
     heartbeats, pulling no bytes, must survive well past the idle timeout. That single test is the
     item's whole justification and it must fail if someone later makes the reaper byte-only.
  5. Rate-limit the media-plane form per session. The control-plane form is behind the gateway and
     already fronted; this one is reachable by anything holding a valid signed URL, and a malfunctioning
     receiver must not be able to hammer it.
  6. Fail-closed is inherited, not softened: with no signing key the media plane does not start
     (MDLV-04 step 6), and that includes this route. **Cast is not an exception to it.**

  ## TEST PLAN
  - `cargo test`: a valid signed URL's `exp`/`sig` authenticates a heartbeat; a tampered or expired
    signature is `403`; a valid signature on a reaped session is `410`
  - `cargo test`: **a session kept alive by heartbeats alone, serving no bytes, survives past
    `MAESTRO_SESSION_IDLE_TIMEOUT_SECS`** (injected clock) — the pause case
  - `cargo test`: the media-plane and control-plane heartbeats produce byte-identical `HeartbeatAck`
    values for the same session state, asserted against one another rather than against a fixture
  - `cargo test`: a heartbeat near expiry returns a renewed `stream_url` with a later `exp` on **both**
    planes — the renewal path is not control-plane-only
  - `cargo test`: no second signer, no second canonical format, no new token type exists in the tree
    (assert the auth module exposes exactly D's surface)
  - `cargo test`: the media-plane heartbeat is rate-limited per session
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Heartbeat racing the reaper → `410`, and close stays idempotent (MDLV-01) — inherited, not re-solved
  - A renewed URL issued to a receiver that never adopts it → harmless; the old URL stays valid until
    `exp`, which is what makes adoption safe to be best-effort
  - Both planes heartbeating one session (a sender and a receiver both alive) → last write wins on
    position; this is legitimate during a handoff and must not be treated as an anomaly
  - Signing key unset → the media plane does not start at all; cast capability reporting is moot
    because there is no media plane to report on

- **Acceptance criteria:**
  - [ ] `POST /playback/{id}/heartbeat` is accepted on the media plane, authenticated by the signed URL
  - [ ] It is the SAME handler as the control-plane form, with identical `HeartbeatAck` output, proven by test
  - [ ] **A paused session surviving on heartbeats alone outlives the idle timeout**, proven by test
  - [ ] A renewed `stream_url` is returned on the media plane exactly as on the control plane
  - [ ] `410` on a reaped session is preserved and distinguishable from `403`
  - [ ] NO new signer, token type, canonical format, key, or credential is introduced — proven by test
  - [ ] The media-plane form is rate-limited per session
  - [ ] MDLV-04's fail-closed behaviour is unchanged; cast is not an exception
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-05: The receiver application — TypeScript, CAF, load, refresh, and legible failure
- **Priority:** Critical
- **Labels:** maestro, cast, receiver, typescript
- **Agent:** claude
- **Estimate:** 10h
- **Blocked by:** MCST-03, MCST-04
- **Description:** The application that runs on the Cast device. A new `cast-receiver/` subtree in the
  Muse repo: TypeScript, Vite, built to a committed `dist/`, embedded by MCST-03. It loads media from
  Maestro, handles the media namespace, keeps its stream token fresh per §4c, and — the part that
  determines whether this is debuggable at all — **shows a legible reason on screen when something
  fails**, because a Cast device has no console anyone will be reading.

  **Original code only (§1b).** Built on Google's CAF receiver SDK and its published documentation.
  Jellyfin's receiver is GPL-2.0 and must not be read or copied. Non-obvious behaviours carry a code
  comment citing the Cast documentation section they came from.

  **Keep it small.** This is a media player on a constrained device: no framework, no state library,
  no router, no design system. Plain TypeScript modules and a handful of DOM nodes. Every dependency
  added here runs on hardware we cannot profile or debug.

  ## FILES
  - `cast-receiver/package.json`, `tsconfig.json`, `vite.config.ts` — new subtree
  - `cast-receiver/index.html` — the CAF SDK `<script>` + the receiver's root
  - `cast-receiver/src/main.ts` — context setup, options, start
  - `cast-receiver/src/load.ts` — the LOAD request interceptor (where the tokens arrive)
  - `cast-receiver/src/renewal.ts` — pure: adopt an ack-supplied URL, classify D's status codes
  - `cast-receiver/src/playlist.ts` — pure: the VOD-vs-EVENT reload decision (§4c)
  - `cast-receiver/src/errors.ts` — pure: error classification → an on-screen message
  - `cast-receiver/src/ui.ts` — the minimal on-screen states
  - `cast-receiver/dist/**` — **committed**
  - `README.md` — the subtree, the build, and the committed-dist rule

  ## APPROACH
  1. `CastReceiverContext` with a **LOAD interceptor**. The sender's LOAD request carries
     `customData: { session_id, stream_url, heartbeat_url }` — **one credential, the signed URL**
     (§4a). The receiver **composes no URL from a host, port or scheme**, exactly the discipline
     MPLY-03 set for the browser. It received a URL; it plays it. A receiver that builds URLs is a
     receiver that will one day build the wrong one.
  2. `renewal.ts` is **pure and unit-tested**, and it is deliberately small because §4b removed the
     arithmetic: the receiver does **not** compute when to renew — it does not know the TTL and should
     not. It answers only "did this ack carry a `stream_url`, and if so what must be re-pointed", plus
     the classification of D's statuses. Renewal timing is MDLV-08's, server-side, where it belongs.
  3. **Error handling straight from D's table (§4c.5).** `403` ⇒ no detail is coming; one heartbeat
     attempt to obtain a renewed URL, then terminal. `410` ⇒ the session is gone; **stop immediately,
     never retry** — only the sender can open a new one. Never a retry loop: a Cast device in a retry
     loop is invisible and will sit there all night on a television nobody is watching.
  4. **The VOD/EVENT rule (§4c), and it gets its own module because it is the subtle one.** On each
     refresh: if the media playlist is EVENT, the reload the player already performs carries the new
     tokens and nothing more is needed. If it is VOD (`#EXT-X-ENDLIST` seen), **explicitly re-fetch the
     media playlist with the new token and hand the new URIs to the player.** A receiver that refreshes
     its token but never re-renders the playlist has refreshed nothing, and the film still dies.
  5. `errors.ts` classifies into the small set that are actually distinguishable and actually
     actionable — token expired and unrefreshable, session gone (`410`), media unsupported by this
     device, network unreachable, transcode stalled — and renders each as a sentence on screen with the
     session id. **The session id on screen is the single highest-value debugging affordance in this
     spec**: it is what lets an operator looking at a stuck TV find the server-side session in one
     step instead of guessing from timestamps.
  6. Idle/on-screen states from the design tokens' palette values transcribed as constants (the
     receiver cannot import constellation-web's CSS), so it looks like part of the system rather than a
     default Cast screen.
  7. Build with Vite to `cast-receiver/dist/` and **commit it**. There is no npm step in the OCI
     publish (§3b) — an uncommitted `dist/` means the device keeps running the old receiver, which is
     considerably harder to notice than a panel that fails to change.

  ## TEST PLAN
  - `npm run typecheck` and `npm run build` in `cast-receiver/`
  - vitest (pure): an ack carrying a `stream_url` is adopted and re-points playback; an ack without one
    changes nothing
  - vitest (pure): the receiver computes no renewal schedule and reads no TTL (asserted on the module's
    surface — timing is server-directed, §4c.1)
  - vitest (pure): error classification maps `403`/`410`/`404` to distinct on-screen messages, and an
    unknown status renders verbatim rather than being coerced to a known one
  - vitest (pure): the VOD playlist triggers an explicit reload; the EVENT playlist does not
  - vitest (stubbed `cast.framework`): a LOAD carrying a signed URL starts heartbeating on the ack's
    `interval_secs`; a LOAD without one renders "cannot authenticate" and does not attempt playback
  - vitest (stubbed): **heartbeats continue while paused** (§4c.2) — the inverse of MPLY-08's browser rule
  - vitest (stubbed): a `410` renders the terminal session-gone state and stops the loop; a `403`
    attempts exactly one renewal then goes terminal
  - vitest: the receiver never constructs a URL from a host/port/scheme (assert on the module, and
    grep-enforce in review)
  - `cast-receiver/dist/` rebuilt and committed in the same change
  - Verify no hardcoded infrastructure values, hostnames or tokens in new/modified files

  ## EDGE CASES
  - LOAD arrives with no heartbeat URL (a sender that predates MCST-08) → play, and render at the first
    `403`/`410` that the session could not be kept alive, rather than an unexplained stop
  - Device suspended past the idle timeout → the session is already reaped; the `410` state is correct
    and terminal, and the sender re-opens. Do not attempt to resurrect it
  - The exchange endpoint is unreachable (Maestro restarted) → retry with backoff for a bounded window,
    then a terminal state naming the reason — never a silent stall
  - Media element error the device reports without detail → render the raw code; never invent a cause
    (the MPLY-12 rule, applied on the device)
  - Sender disconnects → **playback continues**; this is the requirement §4b exists to satisfy and it
    gets an explicit test in MCST-11

- **Acceptance criteria:**
  - [ ] A `cast-receiver/` TypeScript subtree builds to a committed `dist/` embedded by MCST-03
  - [ ] The receiver plays media from a LOAD-supplied URL and composes no URL itself, proven by test
  - [ ] A renewed `stream_url` from a heartbeat ack is adopted; the receiver computes no renewal timing itself
  - [ ] Heartbeats continue while paused, proven by test (§4c.2)
  - [ ] A VOD playlist is explicitly reloaded on refresh; an EVENT playlist is not — proven by test
  - [ ] `410` is terminal and `403` attempts exactly one renewal; neither loops
  - [ ] The receiver holds exactly ONE credential — the signed URL — and no second token type exists
  - [ ] Every failure renders a legible on-screen sentence including the session id
  - [ ] No GPL-derived code; non-obvious behaviours cite the Cast documentation in comments
  - [ ] No framework or state library is added to the receiver
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-06: Receiver playback-state reporting into Server Activity
- **Priority:** High
- **Labels:** maestro, cast, receiver, activity
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MCST-05
- **Description:** Make a cast session look like every other session. The receiver reports position and
  transport state back to Maestro, which feeds it into the existing session record and therefore into
  spec H's Activity panel and the Maestro → Muse event path (spec D MDLV-08) that watch state and taste
  sit on.

  **This is not cosmetic.** Epic §10b is explicit that a lost stop event corrupts watch duration, which
  corrupts taste — the one failure that damages the product silently instead of visibly breaking it. A
  cast session is *more* exposed to this than a browser session, because the thing most likely to end
  it is someone pressing a button on a physical remote, with no page lifecycle event anywhere.

  **The receiver gets no new credential for this**, and it is the same route MCST-04 opened: D's
  heartbeat, accepted on the media plane and authenticated by the signed URL the receiver already
  holds. One credential, one lifetime, one thing to reason about. Note the consequence — **this item
  and MCST-04 are two halves of one mechanism**: MCST-04 makes the route reachable, this item makes the
  receiver actually drive it, and §4c.2's pause-survival needs both.

  ## FILES
  - `cast-receiver/src/heartbeat.ts` — new: the reporting/liveness loop
  - `cast-receiver/src/main.ts` — wire player-manager events to it
  - `src/maestro/http/heartbeat.rs` — accept a stream-token-authenticated heartbeat
  - `src/maestro/http/mod.rs` — route registration
  - `cast-receiver/dist/**`
  - `README.md` — document cast-session reporting

  ## APPROACH
  1. Subscribe to the CAF `PlayerManager` events — playing, paused, seeked, ended, and the
     device-initiated stop — rather than polling. Device-initiated events are the ones a browser
     session has no analogue for and the ones most likely to be missed.
  2. Heartbeat on the cadence the ack dictates (`interval_secs`, server-directed per MDLV-08), so
     Activity's staleness logic needs no cast-specific branch. Carry position and state in D's existing
     `HeartbeatRequest` shape — two shapes for one fact is how the Activity panel grows a backend `if`.
     **Heartbeat while paused as well as while playing** (§4c.2): unlike the browser, the receiver's
     heartbeat is the only thing keeping a paused session out of the reaper.
  3. **Report a stop on every ending path**: `ended`, an explicit stop from the sender, a stop from the
     device's own remote, and the receiver shutting down (`onStop` / the CAF shutdown hook). Idempotent
     — a stop that arrives twice is `Ok`, exactly as spec D's `close()` already is.
  4. Mark the session's client kind as cast, with the device's friendly name, so Activity can say
     *"playing on the living-room TV"* rather than showing an anonymous session. The name comes from
     the Cast framework verbatim; never inferred, never prettified.
  5. A failed heartbeat **never interrupts playback** — log it, retry on the next tick. Spec D's idle
     sweep is the backstop for a receiver that dies without reporting, which is why the sweep exists.
  6. Server-side: the heartbeat route reuses spec D's existing session-update path. **No new session
     state machine for cast** — a second lifecycle would be a second source of truth about what is
     playing, which is the drift epic §2 forbids.

  ## TEST PLAN
  - vitest (stubbed framework): each player event produces the expected report; `ended` produces a stop
  - vitest: a device-initiated stop produces a stop report, not merely a paused report
  - vitest: a failed heartbeat does not stop playback and retries on the next tick
  - vitest: stop is idempotent — two stops produce no error
  - `cargo test`: covered by MCST-04 — not re-tested here
  - `cargo test`: a cast heartbeat updates the SAME `playback_sessions` row a browser heartbeat would;
    there is no second table and no second state machine
  - Live loopback (MCST-10): a driven receiver appears in `GET /playback/sessions` with its device name
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Device unplugged mid-film → no stop arrives; spec D's idle sweep closes it with `idle_timeout`,
    which is a real and useful signal, not a defect
  - Sender disconnects but playback continues → heartbeats continue from the receiver; Activity keeps
    showing it, which is the correct and desired behaviour
  - Position reported while the device buffers → `buffering`, distinct from `paused`, so Activity does
    not misreport a stall as a user action
  - Two receivers on one session (should be impossible) → the later heartbeat wins; log it as an anomaly

- **Acceptance criteria:**
  - [ ] A cast session appears in `GET /playback/sessions` with a live position and the device's name
  - [ ] A stop is reported on end, sender stop, device-remote stop, and receiver shutdown — idempotently
  - [ ] Cast heartbeats update the same session row and state machine as browser heartbeats, proven by test
  - [ ] Heartbeats use MCST-04's media-plane route and the existing signed URL; no second credential exists
  - [ ] A paused cast session keeps heartbeating and is not reaped, confirmed end-to-end in MCST-10
  - [ ] A failed heartbeat never interrupts playback
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-07: Castability — capability reporting and the honest "you cannot cast this" surface
- **Priority:** High
- **Labels:** maestro, cast, capability
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MCST-04
- **Description:** Implement §5. Maestro reports whether casting is possible **at all** (App ID
  configured, TLS configured, signer configured) and whether it is possible **for this item** (the
  resolved backend serves bytes, the plan is servable today). Both as data the GUI branches on — never
  a backend-name check in the client, per epic §8.6's `BackendCaps` rule.

  **The failure this prevents is specific and nasty.** Under the `plex` backend, Maestro never sees a
  byte (epic §8.6), so there is nothing to cast — but every other part of the experience looks
  identical to a `native` item. Without this item, the Cast entry appears, the user picks a device, and
  nothing happens. That is the worst class of bug in a media product: it looks like the file is broken.

  ## FILES
  - `src/maestro/backends/caps.rs` — extend `BackendCapabilities` with `cast_receiver`
  - `src/maestro/http/plan.rs` — the per-item `castable` verdict + reason on the plan response
  - `src/maestro/config.rs` — the composite cast-availability gate
  - `README.md` — document castability and its reasons

  ## APPROACH
  1. `BackendCapabilities.cast_receiver: bool` is **true only for a backend that serves bytes** — i.e.
     `native` today. The plex adapter reports `false` with the reason *"Plex serves its own media;
     Maestro never holds the bytes"*. Derived from the `MediaSource` facet's presence (epic §8.6), not
     from a backend-name match, so a future byte-serving backend gets it automatically.
  2. A **server-level** gate too: App ID and TLS must both be configured. Report which one is missing —
     *"cast unavailable: no App ID configured"* is a one-minute fix; *"cast unavailable"* is an
     afternoon. **The signing key is deliberately not in this list**: per MDLV-04 its absence stops the
     media plane from starting at all (§4a), so there is no running media plane to report cast
     availability on. Do not soften D's fail-closed into a capability flag.
  3. The plan response (spec C's `/playback/plan`) gains `castable: bool` and, when false, a **reason
     from the same structured vocabulary spec C already emits**. Not a new parallel reason system —
     spec C's `TranscodeReason`/`Undecidable` conventions are extended, so MPLY-12's `/why` card renders
     a cast refusal with the same verbatim-reasons rule it already applies to everything else.
  4. Transcode-tier items are `castable: false` with *"needs the transcode path"* until spec E lands,
     then true. Driven by the tier and the server's capability, **never a hardcoded date or flag** — it
     must flip on its own when E deploys, with no change here.
  5. **Never conflate the two cast paths.** A Chromecast running the *Plex* receiver is a controllable
     Plex client through the existing `CastController` seam and stays in MPLY-09's target list under its
     own label. Our receiver is a separate target kind. The capability payload distinguishes them, and
     MCST-08 renders them distinctly.

  ## TEST PLAN
  - `cargo test`: the plex backend reports `cast_receiver: false` with its reason; native reports true
  - `cargo test`: each missing server-level prerequisite produces its own distinct reason string
  - `cargo test`: a transcode-tier plan is `castable: false` with the transcode reason while E's
    capability is absent, and true when present — with no code change between the two cases
  - `cargo test`: `castable` is derived from the `MediaSource` facet, not a backend-name comparison
    (asserted by adding a fake byte-serving backend in the test and getting `true`)
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Backend resolves per-request (epic §10b's A/B routing) → castability is per-request too, never cached across backends
  - Item castable at plan time, backend switched before session open → session open refuses with the same reason vocabulary
  - Plan is `Undecidable` → `castable: false` carrying spec C's own reason verbatim, never a substituted one
  - Cast configured but zero devices on the network → that is a sender-side condition (MCST-08), not a capability; the server still reports available

- **Acceptance criteria:**
  - [ ] `BackendCapabilities.cast_receiver` is derived from byte-serving capability, not a backend name
  - [ ] The plex backend reports cast unavailable with the §5 reason
  - [ ] Each missing server prerequisite produces its own distinct, actionable reason
  - [ ] A per-item `castable` verdict with a spec-C-vocabulary reason appears on the plan response
  - [ ] A transcode item becomes castable when spec E deploys, with no code change in this spec
  - [ ] Plex-receiver casting and Maestro-receiver casting are distinguishable in the payload
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-08: Sender integration in constellation-web — discovery, launch, transport
- **Priority:** High
- **Labels:** maestro, constellation-web, cast, sender
- **Agent:** claude
- **Estimate:** 7h
- **Blocked by:** MCST-05, MCST-07; spec G MPLY-09 (`useTransport`, the target menu)
- **Repository:** `moosenet/Terminus`, subtree `constellation-web/`
- **Description:** Adds Cast as a target kind in MPLY-09's existing menu: device discovery through the
  Cast Web Sender SDK, launching our receiver by App ID, handing over the session credentials, and
  driving transport. **This supersedes spec G's MPLY-10** (§2) and carries forward its design intent
  unchanged where it was right.

  **It adds one `useTransport` implementation and nothing else.** If this item finds itself editing
  `PlayerControls.tsx`, MPLY-09's abstraction was wrong and the fix belongs there — G's own rule,
  restated because a cast transport is exactly the pressure that breaks a leaky abstraction.

  ## FILES
  - `constellation-web/src/panels/maestro/cast/sdk.ts` — new: lazy SDK load, availability, guards
  - `constellation-web/src/panels/maestro/cast/transport.ts` — new: the `useTransport` implementation
  - `constellation-web/src/panels/maestro/TargetMenu.tsx` — the conditional Cast entries
  - `constellation-web/src/hooks/useMaestro.ts` — cast-session start (declares a cast target)
  - `constellation-web/README.md` — the App ID prerequisite and the two distinct cast paths
  - `constellation-web/dist/**`

  ## APPROACH
  1. **The App ID comes from Maestro's capability payload** (MCST-07 → MPLY-01's
     `useMaestroCapabilities`), **never a build-time constant and never a literal in source** — it is
     deployment configuration, and hardcoding it is the class of thing S1 exists to stop. MPLY-10 had
     this right; it is carried over verbatim.
  2. The Cast Sender SDK loads **lazily and only when an App ID is present**. It is an external script
     the shell otherwise never loads; if CSP or the network blocks it, the Cast entry becomes
     unavailable with that reason and nothing else in the section degrades.
  3. Starting a cast session calls session-open and forwards **exactly what D returned** —
     `session_id` and the signed `stream_url` — plus the media-plane heartbeat URL, in the LOAD
     request's `customData`. There is one credential and no token exchange (§4a). **The sender composes
     no media URL either**; it forwards what session-open gave it. The URL must be reachable *from the
     receiver's network position*, which is not the browser's — that is what D's
     `MAESTRO_PUBLIC_BASE_URL` is for. If session-open returns no `stream_url` (a `plex` session
     carries `playback_mode: "backend_controlled"` and `stream_url: null` by D's §2), the Cast entry is
     unavailable with that reason rather than casting something that will fail on the device (MPLY-10's
     rule, kept — and now backed by an explicit field rather than an inference).
  4. One more `useTransport` implementation: play/pause/seek/stop/state over the Cast session. Zero
     changes to `PlayerControls`.
  5. **Two distinct entries, per §5 and MCST-07.** "Cast (Maestro)" for our receiver, and the existing
     Plex-client targets under their own labels. An item that is not castable renders its entry
     disabled **with the plan's reason verbatim** — the MPLY-12 discipline, so a user learns *"Plex
     serves its own media"* instead of watching nothing happen.
  6. Do not re-derive Chromecast's format matrix client-side. Pass the target's profile to Maestro and
     let spec C's `plan()` decide (MPLY-10's rule, kept).
  7. Cast unavailable at the server level → the entry is absent with the server's reason, and the SDK
     is never loaded at all.

  ## TEST PLAN
  - `npm run typecheck`, `npm run build` (passes `assert-http-bundle`), `npm run lint:adherence` with no
    new warnings
  - vitest: with no App ID in the capability payload, no Cast entry appears and the SDK is never loaded
  - vitest: an SDK load failure marks Cast unavailable and leaves every other target working
  - vitest: a `castable: false` item renders its entry disabled with the reason **verbatim**
  - vitest: a `backend_controlled` session (`stream_url: null`) marks Cast unavailable with that reason
  - vitest: the cast transport satisfies the same `useTransport` interface, with no import from `PlayerControls`
  - Live capture with cast unconfigured: no Cast entry, and no external script request in the trace
  - **End-to-end casting is NOT verified by this item** — it is MCST-11's, and the PR must say so
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - No Cast devices on the network → the entry is present but empty-with-a-reason, not an error
  - Receiver rejects the media → surface the receiver's own error text, never a substituted one
  - Cast session ends on the device → the UI returns to "This browser" at the last known position
  - A second sender → the device reports busy and the UI says so
  - A viewer-role user → gated by `RoleGate` **and** refused server-side, exactly as MPLY-09 requires;
    verify the server actually returns `403` and record the observed status

- **Acceptance criteria:**
  - [ ] A "Cast (Maestro)" target appears only when the server reports cast available AND the item is castable
  - [ ] The App ID is read from the capability payload, never hardcoded in source
  - [ ] The SDK loads lazily and never loads when no App ID is configured
  - [ ] The sender forwards D's signed `stream_url` and the heartbeat URL; it composes no media URL and mints no token
  - [ ] A non-castable item's entry is disabled with the plan's reason verbatim
  - [ ] Maestro-receiver casting and Plex-client casting are visibly distinct entries
  - [ ] The cast transport adds NO branching to `PlayerControls`
  - [ ] The PR states plainly that end-to-end casting is unverified pending MCST-11
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-09: Handoff — local ⇄ cast mid-stream, and surviving the sender leaving
- **Priority:** High
- **Labels:** maestro, constellation-web, cast, session
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MCST-08
- **Repository:** `moosenet/Terminus`, subtree `constellation-web/`
- **Description:** Moving playback between the browser and the TV without losing the position — and,
  the harder half, **playback continuing when the sender goes away**. This is the item that decides
  whether casting feels like a feature or like a demo.

  Two directions and one survival property:
  - **Local → cast.** Stop the local element cleanly, carry the position, start a cast session at that
    position, hand over the tokens.
  - **Cast → local.** Stop the cast session, resume locally at the receiver's last reported position.
  - **Sender leaves.** Close the tab and **the film keeps playing.** That is the entire reason MCST-04
    gives the receiver a refresh grant instead of relying on the sender (§4b), and it is the property
    most likely to be broken by an over-eager cleanup handler.

  **The trap here is MPLY-08's `pagehide` teardown**, which correctly stops a session on tab close. For
  a cast session that is exactly wrong: it would stop the film when the laptop closes. The teardown must
  therefore be **session-kind aware**, and that distinction is this item's core deliverable.

  ## FILES
  - `constellation-web/src/panels/maestro/useSession.ts` — session-kind-aware teardown
  - `constellation-web/src/panels/maestro/cast/handoff.ts` — new: the pure position-carry logic
  - `constellation-web/src/panels/maestro/PlayerPanel.tsx` — the handoff affordance
  - `constellation-web/README.md` — document handoff and the teardown asymmetry
  - `constellation-web/dist/**`

  ## APPROACH
  1. `handoff.ts` is **pure and unit-tested**: given source state and a target kind, produce the stop
     and the start with the position to resume at. Position comes from the **media element or the
     receiver's last report**, never from the last heartbeat — which lags by up to one interval and is
     exactly the gap that makes a resume land visibly early (MPLY-08's own lesson).
  2. **The asymmetric teardown, stated as a rule:** a *local* session stops on unmount, route change and
     `pagehide`; a *cast* session stops on **explicit user stop only**. Not on unmount, not on route
     change, not on `pagehide`. Encode it as a property of the session kind so no future handler has to
     remember, and unit-test both directions — a regression here is invisible until someone closes a
     laptop mid-film.
  3. Reconnecting to a running cast session: on mount, if the SDK reports an existing session for our
     App ID, **adopt it** rather than starting a new one. Reopening the tab must show what is playing,
     not start it again.
  4. Local → cast stops the local element **before** starting the cast session, so two sessions never
     serve the same item at once and Activity never shows a phantom.
  5. Cast → local: stop the cast session, then start a local one at the receiver's last reported
     position. If that position is stale (the receiver stopped reporting), say so and resume from the
     last known value rather than from zero.
  6. Errors during handoff must not leave **both** sessions dead. If the start fails, the stop is
     already committed, so the UI offers a plain restart with the carried position rather than a dead
     player and a lost place.

  ## TEST PLAN
  - typecheck + build + `lint:adherence`
  - vitest: `handoff` carries the element's current position, not the last heartbeat's
  - vitest: **`pagehide` does NOT stop a cast session** — the central assertion of this item
  - vitest: `pagehide` DOES stop a local session (no regression to MPLY-08)
  - vitest: an existing cast session is adopted on mount rather than restarted
  - vitest: local → cast stops the local element before the cast session starts
  - vitest: a failed handoff start surfaces a restart affordance with the carried position
  - Live capture: cast → local returns to the browser at the carried position
  - Real-device confirmation of the tab-close survival is **MCST-11's**, and the PR says so
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Handoff while buffering → use the last known good position, never `0`
  - Device disappears mid-cast → offer a local resume at the last reported position (MPLY-09's rule)
  - Two tabs both adopting one cast session → both display it; only an explicit stop stops it
  - Handoff on an item castable locally but not remotely (or vice versa) → the unavailable direction is
    disabled with its reason, never offered and then failed
  - Rapid double handoff → serialized; the second waits for the first to settle

- **Acceptance criteria:**
  - [ ] Local → cast and cast → local both preserve the position, from the element/receiver not the heartbeat
  - [ ] **Closing the tab does NOT stop a cast session**, proven by unit test
  - [ ] Closing the tab DOES still stop a local session — no MPLY-08 regression
  - [ ] An existing cast session is adopted on mount, not restarted
  - [ ] The local source is stopped before the cast session starts; no double session
  - [ ] A failed handoff offers a restart at the carried position rather than a dead player
  - [ ] Embedded `dist` rebuilt and committed
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-10: The stubbed-framework receiver harness (verification layers 2 and 3)
- **Priority:** High
- **Labels:** maestro, cast, testing, harness
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MCST-05, MCST-06
- **Description:** Build §6's layers 2 and 3: a hand-written `cast.framework` test double that lets the
  receiver run in CI, and a driver that runs the same receiver against a **live Maestro** on the harness
  host. Together they prove everything about the cast path except the Cast transport itself and the
  device's decoder — which is a great deal more than "we merged it and hoped".

  **The double is the deliverable, not the tests.** A faithful stub of the small CAF surface the
  receiver actually uses is what makes every future receiver change testable at all. Without it, every
  change to load, refresh or error handling is verifiable only by walking to a television.

  ## FILES
  - `cast-receiver/test/castFrameworkStub.ts` — new: the test double
  - `cast-receiver/test/*.test.ts` — the behavioural suites
  - `cast-receiver/test/live/driver.ts` — new: the live-Maestro driver
  - `cast-receiver/README.md` — how to run both layers, and what each does and does not prove

  ## APPROACH
  1. Stub only what the receiver uses: `CastReceiverContext` (start, options, event subscription),
     `PlayerManager` (LOAD interceptor registration, state, event emission), and the message-bus
     surface. **Do not stub the whole SDK** — a large fake is a second implementation to keep correct
     and will drift into asserting its own behaviour.
  2. The stub records interactions so a test asserts on **what the receiver did**, not on its internals:
     which URL it played, when it refreshed, what it rendered.
  3. The live driver runs the real receiver bundle headless with the stub in place, pointed at a real
     Maestro. Two sequences, and the **first** is the highest-value automated test in this spec:
     - **Pause survival (§4c.2):** open a session, LOAD, pause, serve no bytes, and assert the session
       is still alive well past `MAESTRO_SESSION_IDLE_TIMEOUT_SECS` — then resume and confirm playback.
       This is the defect most likely to ship and it is invisible to every happy-path test.
     - **Renewal:** with `MAESTRO_STREAM_URL_TTL_SECS` lowered, assert an ack delivers a renewed
       `stream_url`, that it is adopted, and that playback is uninterrupted across it.
     Then stop and assert the session closed.
  4. Run it on the existing Playwright harness host (the dev box has no `ffmpeg`/`ffprobe`, epic §11).
     Gate it on a live-Maestro env var and **skip cleanly when unset**, so the CI suite passes with no
     Maestro — the same discipline spec D applies to its database-gated tests.
  5. `cast-receiver/README.md` states plainly what this harness **cannot** prove: the Cast transport,
     the device's decoder, and real network conditions. Layer 4 exists because of those three, and a
     reader must not mistake a green harness for a working TV.

  ## TEST PLAN
  - The stub's own tests: registering a LOAD interceptor, emitting player events, recording calls
  - Receiver suites (from MCST-05/06) run green against the stub in CI
  - Live driver (gated): **paused session survives past the idle timeout** on heartbeats alone, then
    resumes — the §4c.2 case, end to end
  - Live driver (gated): at a lowered TTL, a renewed URL is delivered, adopted, and playback continues
    uninterrupted; then a clean stop, with the session's disappearance from `GET /playback/sessions`
    asserted
  - Live driver: a session closed server-side mid-playback produces the terminal `410` state
  - The suite passes with no Maestro configured (skipped, not failed)
  - Verify no hardcoded infrastructure values

  ## EDGE CASES
  - Maestro restarts mid-run → the driver reports it as an environment failure, distinct from a receiver failure
  - A TTL lowered below the heartbeat interval → renewal fires on every ack; assert it is idempotent
    rather than a re-point storm
  - The stub drifting from the real SDK → the README names the SDK version it mirrors; MCST-11 is the
    only thing that catches real drift, and it says so

- **Acceptance criteria:**
  - [ ] A `cast.framework` test double covering only the surface the receiver uses
  - [ ] Receiver behaviour suites run green in CI with no device and no Maestro
  - [ ] A live driver proves a PAUSED session survives past the idle timeout and resumes (§4c.2)
  - [ ] A live driver proves a renewed URL is adopted mid-playback without interruption
  - [ ] The live layer skips cleanly when Maestro is unconfigured; the suite still passes
  - [ ] The README states explicitly what the harness cannot prove
  - [ ] No hardcoded infrastructure values in new/modified code; all existing tests still pass

### MCST-11: Real-hardware cast verification matrix (operator, recorded)
- **Priority:** Critical
- **Labels:** maestro, cast, operator, verification
- **Agent:** <operator>
- **Estimate:** 3h
- **Type:** human-action
- **Blocked by:** MCST-08, MCST-09, MCST-10
- **Description:** §6's layer 4 — the only layer that can say casting works. Headless CI cannot cast;
  there is no emulator; the receiver runs on hardware we cannot instrument. So this is a human, a
  television, and a written table, and **no item in this spec may claim end-to-end casting before it
  is filled in**.

  It is deliberately a *matrix* rather than "try it once". The defects this spec is most likely to
  ship are conditional — a container one device rejects, an expiry that only bites after fifteen
  minutes, a teardown that only fires when a laptop closes. None of them appear in a five-minute happy
  path, and all of them appear in the table below.

- **Steps:**
  1. Deploy Maestro and the receiver to the Maestro host, and confirm the receiver URL loads over HTTPS
     **from a device on the household network**, not from the dev box.
  2. **Do the pause test first, and do it for real.** Start a film on the device, pause it, and leave
     the room for five minutes. Per §4c.2 a paused receiver pulls no bytes and MDLV-08 reaps after
     ninety seconds, so this is the highest-value test in the spec and no natural happy-path run
     touches it. Then, as a **second** pass, lower `MAESTRO_STREAM_URL_TTL_SECS` so D's renewal path
     actually fires on a device — at the 6h default (§4b) it never would within a normal viewing and
     would ship unexercised.
  3. Run the matrix — **every Cast-capable device the household owns** × these cases:
     | Case | What it proves |
     |---|---|
     | Direct-play item, start → seek → play to end | The base path and range handling |
     | Remux item (spec D tier 2) | The non-seekable pipe on a real device |
     | Transcode item, if spec E has landed | HLS + the playlist token stamping |
     | **Pause for >5 minutes, then resume** | §4c.2's reap survival — the defect most likely to ship |
     | Play past ≥2 renewals at a lowered TTL | D's MDLV-08 renewal actually reaching a device |
     | **Close the sender tab mid-film** | MCST-09's asymmetric teardown; the film must keep playing |
     | Stop from the device's own remote | MCST-06's device-initiated stop reporting |
     | Pause on the device, check Server Activity | The state actually reaching spec H |
     | Attempt to cast a `plex`-backed item | §5's refusal is legible, not a silent nothing |
     | Sleep the device mid-film, wake it | The terminal-`410` path is legible, not a silent stall |
  4. Record **device model, container, tier, outcome and any on-screen message** as a table in the
     Plane item **and** commit it as `docs/cast-verification.md` with the date and the receiver's
     commit SHA — so the next person knows which devices were ever proven, and against which build.
  5. Restore `MAESTRO_STREAM_URL_TTL_SECS` to its default and re-confirm one full playthrough.
  6. Report the outcomes so MCST-08's and MCST-09's deferred verifications can be closed honestly — or
     not closed, if the table says otherwise.

- **Acceptance criteria:**
  - [ ] Every Cast-capable household device tested against every applicable case
  - [ ] A film paused for >5 minutes resumes correctly — the session is not reaped (§4c.2)
  - [ ] A lowered-TTL run survives ≥2 renewals with uninterrupted playback
  - [ ] Closing the sender tab does not stop the film, confirmed on hardware
  - [ ] A `plex`-backed item refuses legibly, with the reason visible to the user
  - [ ] The matrix is committed as `docs/cast-verification.md` with date and receiver SHA
  - [ ] The TTL is restored to its default and one full playthrough re-confirmed
  - [ ] Any failure is recorded as a finding against the owning item, not worked around

---

## 8. Risks

1. **The App ID registration is the schedule risk, and it is external.** Fee, review, propagation
   delay, and device registration — none of it is hard and none of it is under our control. MCST-01 is
   day one for exactly this reason. **Mitigation:** every other item is verifiable without it through
   MCST-10's harness, so a slow registration delays only MCST-11.
2. **The ninety-second pause reap (§4c.2) is the likeliest product-breaking defect**, and every
   natural test misses it: start, watch, stop never pauses for long enough. **Mitigation:** MCST-04's
   media-plane heartbeat, MCST-06 heartbeating while paused, an injected-clock unit test, MCST-10's
   live pause-survival run, and MCST-11's five-minute pause on real hardware. Four layers, because a
   regression here is silent and happens when nobody is in the room.
3. **The VOD playlist trap (§4c) will not be found by any test anyone naturally writes.** A VOD
   playlist is never re-fetched, so its segment signatures must outlive playback or the film dies with
   no reload to save it — after the point where anyone is still watching the test. **Mitigation:** the
   EVENT-while-encoding rule, the explicit reload, a unit test for the decision, and MCST-11's
   lowered-TTL run.
4. **A stale read of a sibling spec produces confident, wrong findings.** This document reported a
   signer gap in spec D that did not exist; the file had been rewritten between the read and the
   report. **Mitigation, and it generalises past this spec:** re-read a sibling spec immediately before
   asserting anything is missing from it, and prefer "K depends on D for X" over "D omits X" — the
   first is true either way, the second is a claim about someone else's file that ages badly.
4. **The certificate is a silent single point of failure.** A renewal that stops working breaks casting
   months later with no other symptom. **Mitigation:** MCST-03 puts days-remaining in the health payload
   so it is visible before it is a problem.
5. **The committed `dist/` will be forgotten at least once.** It already cost the fleet a debugging
   cycle in constellation-web (TERM #550), and here it presents as "the TV is running the old receiver",
   which is harder to spot than a panel that fails to change. **Mitigation:** an acceptance criterion on
   every receiver-touching item, and the receiver rendering its build SHA on the idle screen so an
   operator can read the running version off the television.
6. **Cast looks like it should work for Plex content and does not** (§5). The most likely user-facing
   confusion in the whole epic. **Mitigation:** MCST-07's capability plus MCST-08's verbatim reason —
   refuse legibly, at the point of the attempt.
7. **Sovereignty is genuinely, permanently partial here.** The App-ID→URL resolution is Google's and
   there is no self-hosted substitute. **Mitigation:** confine it to that one lookup (§3b), state it in
   the README rather than letting a reader assume more or less than is true, and keep every byte and
   every piece of metadata on the LAN.

---

## 9. Deliberately out of scope

- **DRM / Widevine.** Nothing in the Constellation serves protected content, and a receiver that
  negotiates DRM is a materially larger application.
- **A multi-bitrate ladder for cast.** Spec E ships one rendition (its §3); the ladder is its own
  follow-up spec and the receiver needs no change for it — the master-playlist URL shape E adopts on
  day one is exactly what makes that true.
- **A Cast-based multi-room / queue experience.** Queues, groups and gapless transitions are a product
  in themselves. One device, one item.
- **Android TV / Apple TV / AirPlay / DIAL-only devices.** Different SDKs, different verification
  matrices, no household hardware to test against today.
- **Voice control and Google Home integration.** Requires publishing the receiver and an assistant
  integration — both of which involve exposing surface beyond the LAN, which is a decision this epic
  has not made.
- **Publishing the receiver publicly.** It stays unpublished and device-restricted; publishing is a
  Google review process serving no household need, and it would make the receiver URL discoverable.
- **Sender-mediated URL renewal.** Explicitly rejected in §4c — it requires the sender to stay
  connected (defeating the whole point of casting) and would not address the pause reap at all.
- **A second token type / refresh-token exchange endpoint.** Drafted for this spec and **dropped**
  (§4a): with D's 6h TTL and D's heartbeat-ack renewal it earned nothing but a second credential to
  keep valid. Recorded so it is not re-proposed as "the obvious missing piece".
- **Any re-implementation of MDLV-04's signing, TTL, key rotation, or error mapping.** D owns all of
  it. An MCST item writing a `sign()` has gone wrong.
- **HDR tone-mapping for cast targets.** Out of scope through spec E by epic §8.3; there is nothing to
  drive.
- **Any change to spec G's player surface.** This spec adds one target kind to an existing abstraction
  (§2). An MCST item that restyles a control or adds a panel is out of scope and should be handed back.
