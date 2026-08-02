# S130 Maestro — reading guide

Thirteen documents. **Read `S130-maestro-epic.md` first and completely** — it constrains every child
spec, and where a child spec disagrees with it, the epic wins.

## Order to read

| Doc | Prefix | What it is |
|---|---|---|
| `S130-maestro-epic.md` | `MSTR` | **Start here.** Architecture, ownership, the decided questions, ground truth. |
| `S130-H-maestro-activity-gui.md` | `MACT` | **Ships first (H1).** Server Activity. Needs zero Maestro. |
| `S130-A-maestro-probe.md` | `MPRB` | Promote Foundry's probe to `src/media/`, then persist + backfill. |
| `S130-B-maestro-backends.md` | `MBAK` | The sidecar, the backend facets, the plex adapter. |
| `S130-C-maestro-decision.md` | `MDEC` | `DeviceProfile` + the pure playback `plan()`. |
| `S130-J-muse-tracker-cutover.md` | `MTRC` | Resolves Plex-session dual ownership. Structural. |
| `S130-D-maestro-delivery.md` | `MDLV` | Direct play, remux, sessions, signed stream URLs. |
| `S130-G-maestro-player-gui.md` | `MPLY` | Player panel. Phase 1 remote control, phase 2 video. |
| `S130-E-maestro-transcode.md` | `MTRX` | HLS, seek, throttle, subtitles. The hard one. |
| `S130-I-maestro-trickplay.md` | `MTRK` | Scrub previews, chapters, keyframe index. |
| `S130-K-maestro-cast-receiver.md` | `MCST` | Chromecast, end to end. Long-lead App ID. |
| `S130-F-maestro-gpu.md` | `MGPU` | Opt-in GPU transcode. **Gated on E's telemetry.** |
| `S130-L-maestro-tuner-serving.md` | `MTUN` | Move linear-channel serving out of the muse process. |

## The five things that matter most

1. **Maestro is a playback abstraction first, a media server second.** One trait family, four
   backends. Muse integrates with an existing server *or* is one — that is the architecture, not a
   compromise.
2. **Same repo, second binary.** Crash isolation is a process boundary, not a repo boundary. Two
   `[[bin]]` targets, two systemd units, two cgroups, one OCI image, one mirror.
3. **Direct play first, transcode last.** Most playback needs no transcoding. Spec A's coverage
   census produces the number that says whether spec E is central or an edge case — measure before
   building.
4. **Foundry already built the probe and the plan engine.** ~8,200 lines on main. Specs A and C
   *promote and extend*; they do not rebuild. See epic §2b.
5. **Ownership is enforced structurally, not socially** — a read-only Postgres role with no access
   to taste tables, a Cargo workspace split, a narrow library view, CI lints. Review rules forget.

## Two decisions that were reversed during authoring

Recorded because the reasoning is the deliverable, and both will otherwise be re-proposed.

- **Maestro will not reverse-proxy Plex's bytes.** Plex mode is control + observe only. Proxying
  means re-streaming against undocumented, token-lifecycle-bound endpoints that shift without
  notice — weeks spent polishing the backend we are replacing.
- **Jellyfin and Emby adapters are deferred**, not cut from the design. No such server is live to
  test against, and an untestable adapter is a liability wearing a feature's badge.

## Standing traps

- **`constellation-web/dist/` is committed and there is no npm step in the OCI publish.** A panel
  change that does not rebuild and commit `dist/` deploys nothing (TERM #550).
- **`ffmpeg`/`ffprobe` are not installed on the dev box.** Gates run on the Muse host or <host>.
- **Fetch before you survey.** A 64-commit-stale checkout hid an entire built subsystem from the
  first half of this epic's authoring.
