# Reference index

One page per major subsystem, derived from the crate's knowledge graph (3,317 nodes; the
node counts below are each subsystem's share) and verified against source. Each page has
a real-symbol table, its cross-subsystem connections, and the configuration keys it
reads.

## Subsystem pages

| Subsystem | Nodes | Source | One-liner |
|---|---|---|---|
| [repo](repo.md) | 279 | `src/repo/` | The sqlx query layer — the only place raw SQL lives; one module per table group |
| [channels](channels.md) | 188 | `src/channels/` | The channel composer/director: deterministic lineups, LLM-optional rationale, presets |
| [models](models.md) | 161 | `src/models/` | Typed rows + `New*` insert structs mapping 1:1 onto the migrations |
| [discord](discord.md) | 126 | `src/discord/` | Discord bot core: allowlisted friends, default-private consent, brain-driven replies |
| [snapshot](snapshot.md) | 123 | `src/snapshot/` | Guarded, read-only snapshot ingestion into an isolated test database |
| [prowlarr](prowlarr.md) | 109 | `src/prowlarr/` | Polite indexer report-pull, deterministic release-name parsing, targeted search |
| [premiere](premiere.md) | 103 | `src/premiere/` | Scheduled premiere events, RSVP, discussion threads, engagement-tiered budgets |
| [cultural](cultural.md) | 90 | `src/cultural/` | Trending ∩ library ∩ taste — the "what's hot / the talk" layer |
| [metadata](metadata.md) | 88 | `src/metadata/` | Provider-agnostic metadata seam + the TheTVDB v4 client |
| [tracker](tracker.md) | 85 | `src/tracker/` | Native Plex playback tracker: webhook + poller + idempotent reconstruction |
| [arr](arr.md) | 84 | `src/arr/` | Read-only multi-instance Radarr/Sonarr ingest + tiered request classification |
| [tuner](tuner.md) | 66 | `src/tuner/` | HDHomeRun emulation + M3U + XMLTV so Plex Live TV tunes Muse channels |

## Subsystems without a dedicated page (yet)

Real, documented modules whose reference material currently lives in their module docs
and the item-level pages below: `web` (85 nodes — guide page, artwork proxy, settings +
graph JSON APIs), `watch_together` (80 — group-session lobby composing personas +
director), `taste_review` (78 — the adversarial reasoning-review panel), plus the
smaller `acquisition`, `decision`, `download`, `library`, `matching`, `curation`,
`taste_model`, `persona`, `embed`, `recall`, `plex`, `streaming`, `maintenance`,
`trending`, `enrichment`, `proactive`, `radar`, `kg`, `promotion`, `conversational`,
`assistant`, `settings`, `adaptation`, `tautulli`, `plex_control`, `shadow`, `parity`,
and `http`. (`endpoint_tests` — 87 nodes — is the `#[cfg(test)]` HTTP harness, not a
runtime subsystem.)

## Item-level pages (from the build sprints)

- [Architecture at a glance](architecture-at-a-glance.md)
- [Running](running.md)
- [HTTP API surface (25 routes)](http-api-surface-25-routes.md)
- [Acquisition domain schema (MUSEM-01)](acquisition-domain-schema-musem-01.md)
- [Release-decision engine](release-decision-engine.md)
- [Acquisition orchestrator + request lifecycle (MUSEM-05)](acquisition-orchestrator-request-lifecycle-musem-05.md)
- [Monitored "wanted" acquisition worker (MUSEM-06)](monitored-wanted-acquisition-worker-musem-06.md)
- [Matching-verification: sample-still extraction (MUSEL-C1)](matching-verification-sample-still-extraction-src-matching-musel-c1.md)
- [Matching-verification: `verify_match` (MUSEL-C2)](matching-verification-verify-match-src-matching-verify-rs-musel-c2.md)
- [Wiring status: what actually runs](wiring-status-what-actually-runs.md)
- [Testing](testing.md)
