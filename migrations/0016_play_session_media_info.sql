-- MUSE-03: play_session_media_info — per-session playback/quality info
-- (spec §3.3, Tautulli session_history_media_info parity). 1:1 owned child
-- of play_sessions -- CASCADE is correct here (unlike play_sessions' own FKs
-- into the library) since this row has no meaning without its session.
CREATE TABLE play_session_media_info (
    play_session_id    bigint PRIMARY KEY REFERENCES play_sessions(id) ON DELETE CASCADE,
    video_decision     decision_kind,
    audio_decision     decision_kind,
    transcode_decision decision_kind,
    container          text,
    video_codec        text,
    audio_codec        text,
    audio_channels     real,
    video_resolution   text,
    bitrate            int,
    width              int,
    height             int,
    transcode_reason   text
);
