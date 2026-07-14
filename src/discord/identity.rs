//! MUSEX-13: the per-Discord-user identity model — DEFAULT-PRIVATE by
//! construction — and the [`TrustedFriends`] allowlist that scopes who the
//! bot serves at all.
//!
//! ## Default-private, enforced by the type system (not just by policy)
//! [`FriendIdentity`]'s consent state — the `taste_opt_in` flag and the
//! linked `muse_account_id` — lives in PRIVATE fields. No code outside this
//! module can write `FriendIdentity { taste_opt_in: true, .. }`, because
//! those fields are unreachable to it. The default is `false`
//! ([`FriendIdentity::new`] and `Default` both produce not-opted-in), and
//! the ONLY production mutator that grants consent is
//! [`FriendIdentity::opt_in`], which sets the flag AND links the account
//! atomically. Reads go through [`FriendIdentity::is_opted_in`] /
//! [`FriendIdentity::linked_account`]. A `#[cfg(test)]`-only
//! [`FriendIdentity::from_parts_for_test`] constructor is the sole way to
//! fabricate an arbitrary consent state (used by one defensive test) — it
//! does not exist in a production build. This is a stronger version of the
//! same "construction proves the invariant" posture `crate::assistant`'s
//! `AskFrequency::Never` short-circuit and `crate::cultural::source::TrendQuery`'s
//! no-PII-egress guarantee use — see `crate::discord::bot` for how "no
//! taste without opt-in" flows from the type signatures, not just a runtime
//! check.

use std::collections::HashMap;

/// One trusted friend's Discord identity, as the bot sees it.
///
/// The two consent fields — `taste_opt_in` and `muse_account_id` — are
/// PRIVATE by design (this is the type-level enforcement codex's review
/// asked for). Consent + account linkage are ONE atomic decision, and the
/// only production code path that grants them is [`Self::opt_in`]; no other
/// module can write `FriendIdentity { taste_opt_in: true, .. }` directly,
/// because those fields are unreachable outside this module's own `impl`.
/// Reads go through [`Self::is_opted_in`] / [`Self::linked_account`].
///
/// `taste_opt_in` gates ALL taste/watch-data use for this friend — see the
/// module doc. `muse_account_id` is `None` until the friend has both opted
/// in AND been linked to a real Muse [`crate::models::account::Account`]
/// (two separate steps folded into one atomic [`Self::opt_in`] call:
/// consenting to taste use, and telling Muse which account's taste to use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FriendIdentity {
    pub discord_user_id: String,
    /// Human-readable label for operator-facing surfaces only (never sent
    /// to Discord as a taste signal, never itself gated by opt-in — a
    /// display name is not watch-data). Public because it carries no
    /// consent semantics.
    pub display_name: String,
    /// PRIVATE. `None` until explicitly linked via [`Self::opt_in`] (see the
    /// struct doc). Read via [`Self::linked_account`]. Even when `Some`,
    /// [`crate::discord::bot::decide_response_mode`] only uses it when
    /// `taste_opt_in` is also `true`.
    muse_account_id: Option<i64>,
    /// PRIVATE. DEFAULT `false`. The single flag that gates taste/watch-data
    /// use for this friend; the ONLY production mutator that sets it `true`
    /// is [`Self::opt_in`]. Read via [`Self::is_opted_in`]. See the module
    /// doc for why this is now provable by construction, not just asserted.
    taste_opt_in: bool,
}

impl Default for FriendIdentity {
    fn default() -> Self {
        Self {
            discord_user_id: String::new(),
            display_name: String::new(),
            muse_account_id: None,
            // Explicit, not merely "happens to be bool::default()" —
            // spelled out so a future field reorder can't silently change
            // this invariant without a reviewer noticing the literal.
            taste_opt_in: false,
        }
    }
}

impl FriendIdentity {
    /// The ONLY way to construct a [`FriendIdentity`] with a real Discord
    /// user id short of `Default` — always starts `taste_opt_in: false` and
    /// `muse_account_id: None`. There is no constructor that takes an opt-in
    /// flag as a parameter, deliberately: opting in is a separate, explicit
    /// act ([`Self::opt_in`]), never a side effect of identity creation.
    pub fn new(discord_user_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            discord_user_id: discord_user_id.into(),
            display_name: display_name.into(),
            muse_account_id: None,
            taste_opt_in: false,
        }
    }

    /// Whether this friend has explicitly consented to taste use. The read
    /// accessor for the private `taste_opt_in` field — the gate
    /// [`crate::discord::bot::decide_response_mode`] consults.
    pub fn is_opted_in(&self) -> bool {
        self.taste_opt_in
    }

    /// The linked Muse account id, if any. The read accessor for the private
    /// `muse_account_id` field. `Some` does NOT by itself authorize taste
    /// use — [`Self::is_opted_in`] must also hold (see
    /// [`crate::discord::bot::decide_response_mode`]).
    pub fn linked_account(&self) -> Option<i64> {
        self.muse_account_id
    }

    /// Explicit opt-in: link a Muse account AND consent to taste use in one
    /// atomic call — the ONLY production mutator that ever sets
    /// `taste_opt_in` to `true`. Because the field is private, this method
    /// is the sole way consent can be granted anywhere outside a
    /// `#[cfg(test)]` build (see [`Self::from_parts_for_test`]).
    #[must_use]
    pub fn opt_in(mut self, muse_account_id: i64) -> Self {
        self.muse_account_id = Some(muse_account_id);
        self.taste_opt_in = true;
        self
    }

    /// Explicit opt-out: symmetric with [`Self::opt_in`] — clears both the
    /// consent flag and the linked account, so a revoked friend has no
    /// residual link an opt-in check could accidentally bypass.
    #[must_use]
    pub fn opt_out(mut self) -> Self {
        self.muse_account_id = None;
        self.taste_opt_in = false;
        self
    }

    /// TEST-ONLY escape hatch to construct an arbitrary consent state —
    /// including the impossible-in-production "opted in but unlinked" state
    /// the defensive `decide_response_mode` test needs to exercise. Gated
    /// behind `#[cfg(test)]` so production code has NO path to set consent
    /// except [`Self::opt_in`]; the private fields stay unreachable to
    /// non-test code. Deliberately NOT `pub` beyond the crate.
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        discord_user_id: impl Into<String>,
        display_name: impl Into<String>,
        taste_opt_in: bool,
        muse_account_id: Option<i64>,
    ) -> Self {
        Self {
            discord_user_id: discord_user_id.into(),
            display_name: display_name.into(),
            muse_account_id,
            taste_opt_in,
        }
    }
}

/// The TRUSTED-FRIENDS allowlist — the scoping gate that runs BEFORE
/// opt-in is even considered. A Discord user id absent from this allowlist
/// is not served at all: no generic reply, no taste reply, nothing (see
/// `crate::discord::bot::ResponseMode::NotServed`).
#[derive(Debug, Clone, Default)]
pub struct TrustedFriends {
    friends: HashMap<String, FriendIdentity>,
}

impl TrustedFriends {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an allowlist from a fixed set of friends (e.g. loaded once at
    /// startup from operator-provisioned config/DB rows — this module
    /// doesn't prescribe the source, only the shape).
    pub fn from_friends(friends: impl IntoIterator<Item = FriendIdentity>) -> Self {
        Self {
            friends: friends
                .into_iter()
                .map(|f| (f.discord_user_id.clone(), f))
                .collect(),
        }
    }

    /// Add or replace a friend's identity (keyed by `discord_user_id`).
    pub fn upsert(&mut self, friend: FriendIdentity) {
        self.friends.insert(friend.discord_user_id.clone(), friend);
    }

    pub fn remove(&mut self, discord_user_id: &str) {
        self.friends.remove(discord_user_id);
    }

    /// Whether this Discord user id is allowlisted at all — independent of
    /// opt-in status. `false` means "not served," full stop.
    pub fn is_allowlisted(&self, discord_user_id: &str) -> bool {
        self.friends.contains_key(discord_user_id)
    }

    /// The allowlisted friend's identity, or `None` if not allowlisted.
    /// This is the ONLY lookup `crate::discord::bot::decide_response_mode`
    /// uses — there is no secondary/bypass lookup path anywhere in this
    /// module.
    pub fn get(&self, discord_user_id: &str) -> Option<&FriendIdentity> {
        self.friends.get(discord_user_id)
    }

    pub fn len(&self) -> usize {
        self.friends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.friends.is_empty()
    }

    /// MUSEX-14: every allowlisted friend who has ALSO opted in — the
    /// enumeration [`crate::promotion::targeting::promote_new_title`] walks
    /// to decide who to target. Filters through [`FriendIdentity::is_opted_in`]
    /// (the same private-field-backed accessor `crate::discord::bot`'s gate
    /// consults), so a non-opted-in friend can never appear here even by a
    /// future refactor mistake — the same "provable by construction" posture
    /// `crate::discord::bot::decide_response_mode` documents for itself.
    /// There is no equivalent "all friends regardless of opt-in" iterator on
    /// this type: every consumer that wants to touch taste-shaped output
    /// must go through this filtered view.
    pub fn opted_in_friends(&self) -> impl Iterator<Item = &FriendIdentity> {
        self.friends.values().filter(|f| f.is_opted_in())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friend_identity_default_is_not_opted_in() {
        let friend = FriendIdentity::default();
        assert!(!friend.is_opted_in());
        assert!(friend.linked_account().is_none());
    }

    #[test]
    fn friend_identity_new_never_opts_in() {
        let friend = FriendIdentity::new("discord-123", "Alex");
        assert!(!friend.is_opted_in());
        assert!(friend.linked_account().is_none());
        assert_eq!(friend.discord_user_id, "discord-123");
    }

    #[test]
    fn opt_in_sets_both_flag_and_account() {
        let friend = FriendIdentity::new("discord-123", "Alex").opt_in(42);
        assert!(friend.is_opted_in());
        assert_eq!(friend.linked_account(), Some(42));
    }

    #[test]
    fn opt_out_clears_both_flag_and_account() {
        let friend = FriendIdentity::new("discord-123", "Alex")
            .opt_in(42)
            .opt_out();
        assert!(!friend.is_opted_in());
        assert!(friend.linked_account().is_none());
    }

    #[test]
    fn opt_in_is_the_only_production_path_that_grants_consent() {
        // Documents the type-level invariant codex's review asked for: the
        // consent fields are private, so the ONLY way production code can
        // reach `is_opted_in() == true` is `opt_in()`. `new`/`Default`
        // always produce not-opted-in; `opt_out()` reverts. There is no
        // public setter and no public struct literal path. (The
        // `from_parts_for_test` escape hatch below is `#[cfg(test)]`-only,
        // so it does not exist in a production build.)
        assert!(!FriendIdentity::new("d", "n").is_opted_in());
        assert!(!FriendIdentity::default().is_opted_in());
        assert!(FriendIdentity::new("d", "n").opt_in(1).is_opted_in());
        assert!(!FriendIdentity::new("d", "n")
            .opt_in(1)
            .opt_out()
            .is_opted_in());
    }

    #[test]
    fn from_parts_for_test_can_build_the_impossible_in_production_state() {
        // The test-only constructor is the sole way to fabricate an
        // "opted-in but unlinked" record (which `opt_in` can never produce),
        // used by the defensive `decide_response_mode` degrade test.
        let friend = FriendIdentity::from_parts_for_test("d", "n", true, None);
        assert!(friend.is_opted_in());
        assert!(friend.linked_account().is_none());
    }

    #[test]
    fn allowlist_scopes_who_is_served() {
        let mut allowlist = TrustedFriends::new();
        assert!(!allowlist.is_allowlisted("discord-123"));
        assert!(allowlist.get("discord-123").is_none());

        allowlist.upsert(FriendIdentity::new("discord-123", "Alex"));
        assert!(allowlist.is_allowlisted("discord-123"));
        assert!(allowlist.get("discord-123").is_some());
        assert!(!allowlist.is_allowlisted("discord-999"));
    }

    #[test]
    fn allowlisted_friends_default_to_not_opted_in() {
        let allowlist = TrustedFriends::from_friends([FriendIdentity::new("discord-123", "Alex")]);
        let friend = allowlist.get("discord-123").expect("allowlisted");
        assert!(!friend.is_opted_in());
    }

    #[test]
    fn opted_in_friends_excludes_not_opted_in_and_not_allowlisted() {
        let allowlist = TrustedFriends::from_friends([
            FriendIdentity::new("discord-opted-in", "Alex").opt_in(1),
            FriendIdentity::new("discord-not-opted-in", "Sam"),
        ]);

        let opted_in: Vec<&str> = allowlist
            .opted_in_friends()
            .map(|f| f.discord_user_id.as_str())
            .collect();

        assert_eq!(opted_in, vec!["discord-opted-in"]);
    }

    #[test]
    fn opted_in_friends_is_empty_for_an_empty_allowlist() {
        let allowlist = TrustedFriends::new();
        assert_eq!(allowlist.opted_in_friends().count(), 0);
    }

    #[test]
    fn remove_takes_a_friend_off_the_allowlist() {
        let mut allowlist =
            TrustedFriends::from_friends([FriendIdentity::new("discord-123", "Alex")]);
        assert!(allowlist.is_allowlisted("discord-123"));
        allowlist.remove("discord-123");
        assert!(!allowlist.is_allowlisted("discord-123"));
    }
}
