# Documentation Index

# Muse

**Muse is an AI-native media curation & taste companion** — a private, local-inference-first
"brain" for a Plex library. It owns taste modeling, curation, metadata, availability intelligence,
proactive recommendations, and a pseudo-TV channel director, backed by a **mandatory Postgres +
pgvector** database, while Plex stays the playback surface and qBittorrent stays the acquisition
tool.

Muse is built **strangler-fig**: it keeps qBittorrent (acquisition) and Plex (consumption) and
owns the *brain* (taste, curation, metadata, release selection, organization). Each phase ships
independent value and *arr/Tautulli are retired one function at a time — import (the only
high-blast-radius part) is dead last. Everything shipped so far is **Phase 0 + Phase 0.5:
read-only, benign playback only, zero blast radius against the live stack**.

It is a peer to the rest of the constellation — **Harmony** (build orchestrator), **Chord**
(inference proxy/orchestrator), **Terminus** (tool hub), and **Lumina** (assistant). Terminus's
`media_*` tools re-point at Muse as phases land, and Lumina consumes Muse's proactive-content
outbox.

See [`specs/S96-muse-foundation.md`](specs/S96-muse-foundation.md) for the founding spec, and the
[`docs/`](docs/) set for grounded reference material:

- [`docs/architecture.md`](docs/architecture.md) — constellation position, data flow, module layering
- [`docs/schema.md`](docs/schema.md) — the Postgres data model, grouped by concern, with the shipped divergences from the spec
- [`docs/runbooks.md`](docs/runbooks.md) — operational runbooks (Tautulli replacement, Prowlarr etiquette, adding Muse as a Plex tuner, the taste/embedding pipeline, the proactive→Lumina contract)
- [`docs/behavior-spec.md`](docs/behavior-spec.md) — the behavioral contract (taste derivation, curation ranking, proactive triggers/cooldowns, the pseudo-TV director, degradation invariants)
- [`docs/EXPERIENCE_LAYER.md`](docs/EXPERIENCE_LAYER.md) — the S118 MUSEX experience layer (personas, channel director, watch-together, adaptation loop, conversational assistant, Discord bot, what's-hot/cultural relevance, KG + graph visualizations, the settings/GUI control panel); documents the opt-in-only privacy model that runs through all of it, and is explicit about which pieces are implemented-and-tested vs. actually wired into a running deployment

> **Accuracy note.** This documentation is written against the shipped code, not the aspirational
> spec. Where a subsystem is a wired-but-untriggered seam or diverges from the founding spec, it is
> marked as such. In particular, three write-path recompute functions (embeddings, taste profile,
> taste-divergence radar) are fully implemented and tested but have **no scheduled worker or route
> calling them yet** — see [Wiring status](#wiring-status-what-actually-runs) below and the docs.

## Contents

- [Architecture at a glance](reference/architecture-at-a-glance.md)
- [Acquisition domain schema (MUSEM-01)](reference/acquisition-domain-schema-musem-01.md)
- [Running](reference/running.md)
- [HTTP API surface (24 routes)](reference/http-api-surface-24-routes.md)
- [Matching-verification: sample-still extraction (`src/matching/`, MUSEL-C1)](reference/matching-verification-sample-still-extraction-src-matching-musel-c1.md)
- [Matching-verification: `verify_match` (`src/matching/verify.rs`, MUSEL-C2)](reference/matching-verification-verify-match-src-matching-verify-rs-musel-c2.md)
- [Release-decision engine](reference/release-decision-engine.md)
- [Acquisition orchestrator + request lifecycle (MUSEM-05)](reference/acquisition-orchestrator-request-lifecycle-musem-05.md)
- [Monitored "wanted" acquisition worker (MUSEM-06)](reference/monitored-wanted-acquisition-worker-musem-06.md)
- [Wiring status: what actually runs](reference/wiring-status-what-actually-runs.md)
- [Testing](reference/testing.md)
