//! MUSEX-13: the per-Discord-user identity model — DEFAULT-PRIVATE by
//! construction — and the [`TrustedFriends`] allowlist that scopes who the
//! bot serves at all.
//!
//! ## Default-private, not "default-private by policy"
//! [`FriendIdentity::taste_opt_in`] is a plain `bool` that `#[derive(Default)]`
//! (Rust's own `bool::default()`) sets to `false`. There is no code path
//! that constructs a [`FriendIdentity`] with `taste_opt_in: true` except an
//! explicit, deliberate field set — [`FriendIdentity::new`] (the only public
//! constructor besides `Default`) hard-codes `false`, and opting in requires
//! calling [`FriendIdentity::opt_in`] as a separate, explicit step. This is
//! the same "construction proves the invariant" posture
//! `crate::assistant`'s `AskFrequency::Never` short-circuit and
//! `crate::cultural::source::TrendQuery`'s no-PII-egress guarantee both
//! use — see `crate::discord::bot` for how this flows into "no taste
//! without opt-in" being provable from the type signatures, not just a
//! runtime check.

use std::collections::HashMap;

/// One trusted friend's Discord identity, as the bot sees it.
///
/// `taste_opt_in` gates ALL taste/watch-data use for this friend — see the
/// module doc. `muse_account_id` is `None` until the friend has both opted
/// in AND been linked to a real Muse [`crate::models::account::Account`]
/// (two separate steps: consenting to taste use, and telling Muse which
/// account's taste to use) — a friend can be opted in with no linked
/// account yet, in which case [`crate::discord::bot::decide_response_mode`]
/// still serves only the generic path, because there is no taste to draw
/// on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FriendIdentity {
    pub discord_user_id: String,
    /// Human-readable label for operator-facing surfaces only (never sent
    /// to Discord as a taste signal, never itself gated by opt-in — a
    /// display name is not watch-data).
    pub display_name: String,
    /// `None` until explicitly linked to a Muse account (see the struct
    /// doc). Even when `Some`, [`crate::discord::bot::decide_response_mode`]
    /// only uses it when `taste_opt_in` is also `true`.
    pub muse_account_id: Option<i64>,
    /// DEFAULT `false`. The single flag that gates taste/watch-data use for
    /// this friend — see the module doc for why this is provable, not just
    /// asserted.
    pub taste_opt_in: bool,
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
    /// user id short of `Default`/struct-update syntax — always starts
    /// `taste_opt_in: false` and `muse_account_id: None`. There is no
    /// constructor that takes an opt-in flag as a parameter, deliberately:
    /// opting in is a separate, explicit act ([`Self::opt_in`]), never a
    /// side effect of identity creation.
    pub fn new(discord_user_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            discord_user_id: discord_user_id.into(),
            display_name: display_name.into(),
            muse_account_id: None,
            taste_opt_in: false,
        }
    }

    /// Explicit opt-in: link a Muse account AND consent to taste use in one
    /// deliberate call — the only way `taste_opt_in` ever becomes `true`.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friend_identity_default_is_not_opted_in() {
        let friend = FriendIdentity::default();
        assert!(!friend.taste_opt_in);
        assert!(friend.muse_account_id.is_none());
    }

    #[test]
    fn friend_identity_new_never_opts_in() {
        let friend = FriendIdentity::new("discord-123", "Alex");
        assert!(!friend.taste_opt_in);
        assert!(friend.muse_account_id.is_none());
        assert_eq!(friend.discord_user_id, "discord-123");
    }

    #[test]
    fn opt_in_sets_both_flag_and_account() {
        let friend = FriendIdentity::new("discord-123", "Alex").opt_in(42);
        assert!(friend.taste_opt_in);
        assert_eq!(friend.muse_account_id, Some(42));
    }

    #[test]
    fn opt_out_clears_both_flag_and_account() {
        let friend = FriendIdentity::new("discord-123", "Alex")
            .opt_in(42)
            .opt_out();
        assert!(!friend.taste_opt_in);
        assert!(friend.muse_account_id.is_none());
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
        assert!(!friend.taste_opt_in);
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
