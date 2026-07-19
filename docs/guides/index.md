# Guides

Task-oriented operator guides. Each names only real binaries, routes, and env keys from
the shipped code; secret values come from the vault at runtime and are never inlined.

| Guide | What you'll do |
|---|---|
| [Add Muse as a Plex tuner](plex-tuner.md) | Point Plex Live TV at Muse's HDHomeRun emulation (or the M3U+XMLTV alternative) and stream composed channels |
| [The acquisition pipeline](acquisition-pipeline.md) | Configure Prowlarr + qBittorrent, submit a media request, and follow it through search → classify → decide → grab |
| [The snapshot / shadow / parity pipeline](snapshot-pipeline.md) | Acquire read-only source snapshots, run the shadow Tautulli-replacement analytics, and produce a retirement-readiness parity report |

Broader operational material (Prowlarr etiquette, the taste/embedding pipeline, the
proactive→Lumina contract) lives in [runbooks.md](../runbooks.md).
