//! MUSEX-14 (Plane TERM #390): the real targeting + dispatch logic behind
//! [`super`]'s module doc.

use sqlx::PgPool;

use crate::curation::candidates::{top_affinity_key, Candidate, CandidateSource};
use crate::curation::recommend::build_rationale;
use crate::discord::bot::{build_taste_reply, BotReply};
use crate::discord::client::DiscordClient;
use crate::discord::identity::TrustedFriends;
use crate::error::MuseResult;
use crate::models::embedding::{EmbeddingEntityKind, DEFAULT_EMBEDDING_MODEL};
use crate::repo;
use crate::taste_model::chord_client::ChordClient;

use super::cosine_similarity_01;

/// One friend targeted by [`promote_new_title`] — carries the exact
/// [`BotReply`] `crate::discord::bot::respond`'s `TasteAware` arm would
/// build for this candidate, so dispatch never re-derives it.
#[derive(Debug, Clone)]
pub struct TargetedPromotion {
    pub discord_user_id: String,
    pub muse_account_id: i64,
    /// The [`super::cosine_similarity_01`] score that cleared the
    /// threshold — kept for logging/audit, not re-derivable from `reply`
    /// alone.
    pub similarity: f64,
    pub reply: BotReply,
}

/// Score a newly-available `media_item_id` against every friend
/// [`TrustedFriends::opted_in_friends`] yields, and return a
/// [`TargetedPromotion`] for each whose match clears `threshold` — see the
/// module doc for the full privacy/reuse contract. A friend below threshold
/// (or with no taste profile/centroid yet — a cold-start account, same
/// graceful-degrade posture as
/// `crate::curation::candidates::gather_taste_candidates`) is simply absent
/// from the result, not an error.
///
/// Returns an empty `Vec` (never an error) when the title itself has no
/// stored [`crate::models::embedding::Embedding`] yet (hasn't been through
/// the MUSE-08 embed pipeline) — there is nothing to score against for
/// anyone this pass; a later pass (once embedded) can promote it.
pub async fn promote_new_title(
    pool: &PgPool,
    friends: &TrustedFriends,
    media_item_id: i64,
    threshold: f64,
    chord: Option<&ChordClient>,
    public_base_url: Option<&str>,
) -> MuseResult<Vec<TargetedPromotion>> {
    let item = repo::media_item::get(pool, media_item_id).await?;
    let meta = repo::media_metadata::get(pool, item.media_metadata_id).await?;

    let Some(embedding) = repo::embedding::get(
        pool,
        EmbeddingEntityKind::MediaItem.as_str(),
        media_item_id,
        DEFAULT_EMBEDDING_MODEL,
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let item_vector = embedding.embedding.as_slice().to_vec();

    let mut out = Vec::new();

    for friend in friends.opted_in_friends() {
        // `linked_account` is always `Some` for anything `opted_in_friends`
        // yields via the production `opt_in()` path (the two fields are set
        // atomically) -- this `let-else` only guards the test-only
        // impossible-in-production state, same defensive posture
        // `crate::discord::bot::decide_response_mode` takes for the
        // identical case.
        let Some(account_id) = friend.linked_account() else {
            continue;
        };

        let Some(profile) = repo::taste::get_profile(pool, account_id).await? else {
            continue;
        };
        let Some(centroid) = &profile.overall_centroid else {
            continue;
        };

        let similarity = cosine_similarity_01(&item_vector, centroid.as_slice());
        if similarity < threshold {
            continue;
        }

        let mut facts = vec![format!(
            "it just landed in the library and it's a {:.0}% match to your taste profile",
            similarity * 100.0
        )];
        if let Some(genre) = top_affinity_key(&profile.genre_affinity) {
            facts.push(format!("you rate {genre} highly"));
        }

        let candidate = Candidate {
            media_metadata_id: meta.id,
            media_item_id: Some(media_item_id),
            title: meta.title.clone(),
            year: meta.year,
            kind: meta.kind,
            source: CandidateSource::Taste,
            taste_fit: similarity,
            facts,
            availability: None,
        };

        let rationale = build_rationale(chord, &candidate).await;
        let reply = build_taste_reply(&candidate, &rationale, public_base_url);

        out.push(TargetedPromotion {
            discord_user_id: friend.discord_user_id.clone(),
            muse_account_id: account_id,
            similarity,
            reply,
        });
    }

    Ok(out)
}

/// Deliver every [`TargetedPromotion`] through a [`DiscordClient`] — a
/// plain reply (the rationale) followed by the rich embed when one was
/// built. Thin glue only: all the targeting/consent logic already happened
/// in [`promote_new_title`], so this function's only job is "send what was
/// decided," making it trivially mockable in tests via
/// `crate::discord::client::MockDiscordClient`.
pub async fn dispatch_promotions(
    discord: &dyn DiscordClient,
    promotions: &[TargetedPromotion],
) -> MuseResult<()> {
    for promotion in promotions {
        discord
            .reply(&promotion.discord_user_id, &promotion.reply.content)
            .await?;
        if let Some(embed) = promotion.reply.embed.clone() {
            discord
                .post_embed(&promotion.discord_user_id, embed)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::client::MockDiscordClient;
    use crate::discord::identity::FriendIdentity;

    fn reply(content: &str, with_embed: bool) -> BotReply {
        BotReply {
            content: content.to_string(),
            embed: with_embed.then(|| crate::discord::client::RichEmbed {
                title: "Severance".to_string(),
                poster_url: None,
                synopsis: "you'll love this".to_string(),
            }),
        }
    }

    #[tokio::test]
    async fn dispatch_promotions_sends_a_reply_and_embed_per_promotion() {
        let mock = MockDiscordClient::new();
        let promotions = vec![TargetedPromotion {
            discord_user_id: "discord-1".to_string(),
            muse_account_id: 1,
            similarity: 0.9,
            reply: reply("You'll love this", true),
        }];

        dispatch_promotions(&mock, &promotions).await.unwrap();

        assert_eq!(mock.reply_call_count(), 1);
        assert_eq!(mock.embed_call_count(), 1);
    }

    #[tokio::test]
    async fn dispatch_promotions_skips_the_embed_call_when_none_was_built() {
        let mock = MockDiscordClient::new();
        let promotions = vec![TargetedPromotion {
            discord_user_id: "discord-1".to_string(),
            muse_account_id: 1,
            similarity: 0.9,
            reply: reply("You'll love this", false),
        }];

        dispatch_promotions(&mock, &promotions).await.unwrap();

        assert_eq!(mock.reply_call_count(), 1);
        assert_eq!(mock.embed_call_count(), 0);
    }

    #[tokio::test]
    async fn dispatch_promotions_handles_multiple_targets_independently() {
        let mock = MockDiscordClient::new();
        let promotions = vec![
            TargetedPromotion {
                discord_user_id: "discord-1".to_string(),
                muse_account_id: 1,
                similarity: 0.9,
                reply: reply("pick one", true),
            },
            TargetedPromotion {
                discord_user_id: "discord-2".to_string(),
                muse_account_id: 2,
                similarity: 0.7,
                reply: reply("pick two", false),
            },
        ];

        dispatch_promotions(&mock, &promotions).await.unwrap();

        assert_eq!(mock.reply_call_count(), 2);
        assert_eq!(mock.embed_call_count(), 1);
        let replies = mock.reply_calls.lock().unwrap();
        assert_eq!(
            replies[0],
            ("discord-1".to_string(), "pick one".to_string())
        );
        assert_eq!(
            replies[1],
            ("discord-2".to_string(), "pick two".to_string())
        );
    }

    #[test]
    fn friend_without_a_linked_account_cannot_be_scored() {
        // Sanity check on the defensive `let-else` above: an impossible-in-
        // production "opted in but unlinked" record must never panic this
        // path -- it's simply skipped, same as the real DB-backed test
        // below would skip a cold-start account with no taste profile yet.
        let friend = FriendIdentity::from_parts_for_test("discord-1", "Alex", true, None);
        assert!(friend.is_opted_in());
        assert!(friend.linked_account().is_none());
    }
}

/// DB-backed end-to-end coverage: real taste centroids + a real embedded
/// title, scored through the actual pgvector-shaped (768-dim) data this
/// crate stores. `db_gated` per `MUSE_TEST_DATABASE_URL`, same convention as
/// `crate::discord::bot::db_gated` / `crate::endpoint_tests::db_gated` —
/// skips cleanly, never a hard failure, when no test database is
/// configured.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::discord::identity::FriendIdentity;
    use crate::models::account::NewAccount;
    use crate::models::embedding::{NewEmbedding, EMBEDDING_DIM};
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use crate::models::taste::NewTasteProfile;
    use pgvector::Vector;
    use serde_json::json;

    /// The default promotion threshold this crate ships
    /// (`Config::promotion_match_threshold`'s default) — duplicated as a
    /// plain constant here rather than constructing a full `Config` just for
    /// this one field, mirroring how other `db_gated` suites in this crate
    /// pin their own local thresholds.
    const TEST_THRESHOLD: f64 = 0.55;

    async fn test_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
            );
            return None;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");
        Some(pool)
    }

    /// A 768-dim (`EMBEDDING_DIM`) one-hot-ish vector, all mass in
    /// `[start, start + width)`. Two vectors built from disjoint ranges are
    /// exactly orthogonal (`cosine_similarity_01` == 0.0); two built from
    /// the SAME range are identical (`cosine_similarity_01` == 1.0) — a
    /// deterministic, seeded way to make "these two tastes genuinely
    /// diverge" a checkable property, not an assumption.
    fn seeded_vector(start: usize, width: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM as usize];
        for slot in v.iter_mut().skip(start).take(width) {
            *slot = 1.0;
        }
        v
    }

    async fn seed_account_with_taste(pool: &sqlx::PgPool, centroid: Vec<f32>) -> i64 {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let account = repo::account::create(
            pool,
            &NewAccount {
                plex_account_id: Some(format!("plex-{suffix}")),
                username: Some(format!("user-{suffix}")),
                friendly_name: Some("Promotion Target Probe".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        repo::taste::upsert_profile(
            pool,
            &NewTasteProfile {
                account_id: account.id,
                genre_affinity: json!({"sci-fi": 3.0}),
                person_affinity: json!({}),
                keyword_affinity: json!({}),
                runtime_pref: None,
                quality_sensitivity: None,
                overall_centroid: Some(Vector::from(centroid)),
                model_notes: None,
            },
        )
        .await
        .expect("create taste profile");

        account.id
    }

    /// Seed one embedded title (a fresh "just landed" library item) with the
    /// given embedding vector, returning `(media_item_id, title)`.
    async fn seed_embedded_title(pool: &sqlx::PgPool, embedding: Vec<f32>) -> (i64, String) {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let title = format!("MUSEX14-PromotionProbe-{suffix}");

        let library = repo::library::create(
            pool,
            &NewLibrary {
                name: format!("lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: format!("/movies-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: Some("a real, seeded synopsis".to_string()),
                studio: None,
                network: None,
                runtime_minutes: Some(110),
                year: Some(2025),
                images: json!({}),
            },
        )
        .await
        .expect("create media_metadata");

        let media_item = repo::media_item::upsert(
            pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/movies-{suffix}/movie.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("plexkey-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("create media_item");

        repo::embedding::upsert(
            pool,
            &NewEmbedding {
                entity_kind: EmbeddingEntityKind::MediaItem,
                entity_id: media_item.id,
                model: DEFAULT_EMBEDDING_MODEL.to_string(),
                dim: EMBEDDING_DIM,
                embedding: Vector::from(embedding),
                source_text: Some("seeded test embedding".to_string()),
            },
        )
        .await
        .expect("create embedding");

        (media_item.id, title)
    }

    #[tokio::test]
    async fn promotion_targets_only_the_friend_whose_taste_actually_matches() {
        let Some(pool) =
            test_pool_or_skip("promotion_targets_only_the_friend_whose_taste_actually_matches")
                .await
        else {
            return;
        };

        // Two disjoint (orthogonal) taste centroids -- genuinely divergent,
        // not just "different objects with the same shape." Sanity-check
        // that divergence directly before relying on it below.
        let camp_a_centroid = seeded_vector(0, 300);
        let camp_b_centroid = seeded_vector(400, 300);
        assert!(
            cosine_similarity_01(&camp_a_centroid, &camp_b_centroid) < 0.01,
            "fixture bug: camp A and camp B centroids must be genuinely divergent"
        );

        let matching_account = seed_account_with_taste(&pool, camp_a_centroid.clone()).await;
        let divergent_account = seed_account_with_taste(&pool, camp_b_centroid).await;

        // The new title's embedding is built from camp A's own range, so it
        // matches the matching account closely and the divergent one not at
        // all.
        let (media_item_id, title) = seed_embedded_title(&pool, seeded_vector(0, 300)).await;

        let friends = TrustedFriends::from_friends([
            FriendIdentity::new("discord-match", "Match").opt_in(matching_account),
            FriendIdentity::new("discord-divergent", "Divergent").opt_in(divergent_account),
        ]);

        let promotions =
            promote_new_title(&pool, &friends, media_item_id, TEST_THRESHOLD, None, None)
                .await
                .expect("promote_new_title should not error");

        assert_eq!(
            promotions.len(),
            1,
            "exactly one friend's taste should clear the threshold, got: {promotions:?}"
        );
        let promoted = &promotions[0];
        assert_eq!(promoted.discord_user_id, "discord-match");
        assert_eq!(promoted.muse_account_id, matching_account);
        assert!(promoted.similarity >= TEST_THRESHOLD);
        assert!(
            promoted.reply.content.contains(&title),
            "the promotion must be grounded in the real seeded title, got: {}",
            promoted.reply.content
        );
        assert!(promoted.reply.embed.is_some());
    }

    /// LOAD-BEARING PRIVACY NEGATIVE TEST. A friend with a REAL, KNOWN-GOOD
    /// taste match (same centroid range as the new title) but who is NOT
    /// opted in must receive zero promotions — proving this isn't merely "no
    /// match was found," but that consent is checked before scoring is even
    /// attempted (the friend never enters `opted_in_friends()` at all).
    #[tokio::test]
    async fn non_opted_in_friend_with_a_known_good_match_gets_zero_promotions() {
        let Some(pool) =
            test_pool_or_skip("non_opted_in_friend_with_a_known_good_match_gets_zero_promotions")
                .await
        else {
            return;
        };

        let matching_vector = seeded_vector(0, 300);
        let matching_account = seed_account_with_taste(&pool, matching_vector.clone()).await;
        let (media_item_id, _title) = seed_embedded_title(&pool, matching_vector).await;

        // Allowlisted (not NotServed) and linked to the real matching
        // account -- but taste_opt_in stays false. Only `from_parts_for_test`
        // can build this state at all; production's `opt_in()` cannot
        // produce "linked but not consented" (see `FriendIdentity`'s own
        // doc). This is deliberately the strictest version of the negative
        // test.
        let friend = FriendIdentity::from_parts_for_test(
            "discord-not-opted-in",
            "Sam",
            false,
            Some(matching_account),
        );
        assert!(!friend.is_opted_in(), "sanity: not opted in");
        let friends = TrustedFriends::from_friends([friend]);
        assert_eq!(
            friends.opted_in_friends().count(),
            0,
            "sanity: the non-opted-in friend must not appear in the opted-in iterator"
        );

        let promotions =
            promote_new_title(&pool, &friends, media_item_id, TEST_THRESHOLD, None, None)
                .await
                .expect("promote_new_title should not error");

        assert!(
            promotions.is_empty(),
            "a non-opted-in friend must receive zero promotions even with a real, known-good \
             taste match: {promotions:?}"
        );
    }

    #[tokio::test]
    async fn cold_start_account_with_no_taste_profile_is_never_promoted_to() {
        let Some(pool) =
            test_pool_or_skip("cold_start_account_with_no_taste_profile_is_never_promoted_to")
                .await
        else {
            return;
        };

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let cold_start_account = repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("plex-{suffix}")),
                username: Some(format!("user-{suffix}")),
                friendly_name: Some("Cold Start Probe".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account")
        .id;

        let (media_item_id, _title) = seed_embedded_title(&pool, seeded_vector(0, 300)).await;

        let friends = TrustedFriends::from_friends([FriendIdentity::new(
            "discord-cold-start",
            "NoProfileYet",
        )
        .opt_in(cold_start_account)]);

        let promotions =
            promote_new_title(&pool, &friends, media_item_id, TEST_THRESHOLD, None, None)
                .await
                .expect("promote_new_title should not error, even with no taste profile yet");

        assert!(
            promotions.is_empty(),
            "an account with no taste profile yet must degrade to no promotion, not an error"
        );
    }
}
