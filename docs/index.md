# Muse — Documentation Index

**Muse is an AI-native media curation & taste companion** — a private,
local-inference-first "brain" for a Plex library. It owns acquisition, library scan,
metadata, availability intelligence, taste modeling, proactive recommendations, and a
pseudo-TV channel director, backed by a **mandatory Postgres + pgvector** database, while
Plex stays the playback surface and qBittorrent stays the download tool.

Muse is built **strangler-fig**: it keeps qBittorrent (downloads) and Plex (consumption)
and owns the *brain* (taste, curation, metadata, release selection, organization). Each
phase ships independent value and *arr/Tautulli are retired one function at a time —
import (the only high-blast-radius part) is dead last. It is a peer to the rest of the
constellation — **Harmony** (build orchestrator), **Chord** (inference
proxy/orchestrator), **Terminus** (tool hub), and **Lumina** (assistant). Terminus's
`media_*` tools re-point at Muse as phases land, and Lumina consumes Muse's
proactive-content outbox.

> **Accuracy note.** This documentation is written against the shipped code, not the
> aspirational spec. Where a subsystem is a wired-but-untriggered seam or diverges from
> the founding spec, it is marked as such. The honest inventory of what actually runs in
> a deployment is [Wiring status](reference/wiring-status-what-actually-runs.md).

## Start here

| Page | What's in it |
|---|---|
| [Getting started](getting-started.md) | Build, configure, first run, verification |
| [Architecture](architecture.md) | Constellation position, process shape, the four data flows, background workers, module layering |
| [Reference index](reference/index.md) | 12 per-subsystem reference pages + the full module inventory |
| [Guides index](guides/index.md) | Operator guides: Plex tuner setup, the acquisition pipeline, the snapshot/shadow/parity CLIs |

## Concept and contract documents

- [schema.md](schema.md) — the Postgres data model, grouped by concern, with the shipped
  divergences from the spec.
- [behavior-spec.md](behavior-spec.md) — the behavioral contract: taste derivation,
  curation ranking, proactive triggers/cooldowns, the pseudo-TV director, degradation
  invariants.
- [runbooks.md](runbooks.md) — operational runbooks (Tautulli replacement, Prowlarr
  etiquette, adding Muse as a Plex tuner, the taste/embedding pipeline, the
  proactive→Lumina contract).
- [EXPERIENCE_LAYER.md](EXPERIENCE_LAYER.md) — the S118 MUSEX experience layer (personas,
  channel director, watch-together, adaptation loop, conversational assistant, Discord
  bot, cultural relevance, KG + graph visualizations, settings control panel) and the
  opt-in-only privacy model that runs through all of it.
- [MUSEX-experience-layer.md](MUSEX-experience-layer.md) — the MUSEX build map the
  experience-layer modules were written against.
- [TESTING.md](TESTING.md) — test strategy: pure-function unit tests plus
  `MUSE_TEST_DATABASE_URL`-gated live-DB tests.

## Subsystem reference

One page per major subsystem, each with a verified symbol table, its cross-subsystem
connections, and the configuration it reads:

- [repo](reference/repo.md) · [models](reference/models.md) ·
  [tracker](reference/tracker.md) · [arr](reference/arr.md) ·
  [prowlarr](reference/prowlarr.md) · [metadata](reference/metadata.md) ·
  [channels](reference/channels.md) · [tuner](reference/tuner.md) ·
  [snapshot](reference/snapshot.md) · [cultural](reference/cultural.md) ·
  [discord](reference/discord.md) · [premiere](reference/premiere.md)

## Item-level reference (from the build sprints)

Deep pages written alongside specific spec items, kept as-is:

- [Architecture at a glance](reference/architecture-at-a-glance.md)
- [Running](reference/running.md)
- [HTTP API surface (25 routes)](reference/http-api-surface-25-routes.md)
- [Acquisition domain schema (MUSEM-01)](reference/acquisition-domain-schema-musem-01.md)
- [Release-decision engine](reference/release-decision-engine.md)
- [Acquisition orchestrator + request lifecycle (MUSEM-05)](reference/acquisition-orchestrator-request-lifecycle-musem-05.md)
- [Monitored "wanted" acquisition worker (MUSEM-06)](reference/monitored-wanted-acquisition-worker-musem-06.md)
- [Matching-verification: sample-still extraction (MUSEL-C1)](reference/matching-verification-sample-still-extraction-src-matching-musel-c1.md)
- [Matching-verification: `verify_match` (MUSEL-C2)](reference/matching-verification-verify-match-src-matching-verify-rs-musel-c2.md)
- [Wiring status: what actually runs](reference/wiring-status-what-actually-runs.md)
- [Testing](reference/testing.md)

## Specs

- [`specs/S96-muse-foundation.md`](../specs/S96-muse-foundation.md) — the founding spec.
- [`specs/S119-muse-media-management.md`](../specs/S119-muse-media-management.md) — the
  acquisition write-path sprint.
- [`specs/S119b-muse-library-scan-matching.md`](../specs/S119b-muse-library-scan-matching.md)
  — library scan + still-frame matching verification.
