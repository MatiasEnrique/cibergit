//! Account-scoped read admission and refresh lifecycles, independent of GPUI.
//!
//! Views own opaque lane handles and describe the current observation identity.
//! This module owns coalescing, pacing, stale completion/cache rejection and fair
//! resumption. A stale payload can still release its account slot and extend a
//! server floor. Mutation authority is deliberately separate.

use cibergit::{
    domain::Account,
    providers::{GeneralReadCache, GeneralReadDelay, GeneralReadDirective},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(super) const RATE_DEFERRED_NOTICE: &str =
    "Refresh deferred by GitHub rate limiting; current data is unchanged.";
pub(super) const RATE_SUSPENDED_NOTICE: &str =
    "Refresh paused by an unrepresentable GitHub rate-limit delay; current data is unchanged.";
pub(super) const POLL_DEFERRED_NOTICE: &str =
    "Refresh deferred by GitHub's polling interval; current data is unchanged.";
pub(super) const PROVIDER_UNAVAILABLE_NOTICE: &str =
    "GitHub read unavailable; historical data is unchanged.";
pub(super) const DATA_INCOMPLETE_NOTICE: &str =
    "GitHub returned incomplete read data; current data is unchanged.";

static NEXT_LIFETIME: AtomicU64 = AtomicU64::new(1);

/// A refresh destination. Allocate a new handle when a tab or repository is
/// replaced, even when it represents the same remote resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RefreshLane(u64);

impl Default for RefreshLane {
    fn default() -> Self {
        Self(NEXT_LIFETIME.fetch_add(1, Ordering::Relaxed))
    }
}

/// Facts the view owns which can invalidate an in-flight observation. The
/// revision is an observation generation, not a commit SHA. Changing it does
/// not release the running task's account slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RefreshContext {
    pub workspace: u64,
    pub repository: String,
    pub resource: Option<u64>,
    pub revision: u64,
}

pub(super) struct RefreshAdmission {
    token: RefreshToken,
    cache: GeneralReadCache,
}

impl RefreshAdmission {
    pub(super) fn into_parts(self) -> (RefreshToken, GeneralReadCache) {
        (self.token, self.cache)
    }
}

pub(super) struct RefreshToken {
    lane: RefreshLane,
    context: RefreshContext,
    read: ReadToken,
}

impl RefreshToken {
    pub(super) fn lane(&self) -> RefreshLane {
        self.lane
    }
}

/// Only a completion that released this controller's operation may resume work.
/// Foreign/duplicate callbacks must not trigger work in a replacement workspace.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefreshCompletion<T, E> {
    Ignored,
    Discarded,
    Applied(Result<T, E>),
}

enum RefreshState {
    Active { explicit_pending: bool },
    Deferred { explicit: bool },
}

impl RefreshState {
    fn has_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloorReason {
    PollInterval,
    RateLimit,
}

#[derive(Default)]
struct ServerFloor {
    not_before: Option<Instant>,
    suspended: bool,
    reason: Option<FloorReason>,
}

impl ServerFloor {
    fn apply(
        &mut self,
        delay: &GeneralReadDelay,
        reason: FloorReason,
        now: Instant,
        wall_now: SystemTime,
    ) {
        let deadline = match delay {
            GeneralReadDelay::Seconds(seconds) => now.checked_add(Duration::from_secs(*seconds)),
            GeneralReadDelay::UntilUnixSeconds(unix) => wall_now
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|wall| unix.saturating_sub(wall.as_secs()))
                .and_then(|seconds| now.checked_add(Duration::from_secs(seconds))),
            GeneralReadDelay::Suspend => None,
        };
        let Some(deadline) = deadline else {
            self.suspended = true;
            self.reason = Some(reason);
            return;
        };
        if self.not_before.is_none_or(|current| deadline > current) {
            self.not_before = Some(deadline);
            self.reason = Some(reason);
        }
    }

    fn notice(&self, now: Instant) -> Option<&'static str> {
        if self.suspended {
            return Some(RATE_SUSPENDED_NOTICE);
        }
        let active = self.not_before.is_some_and(|deadline| deadline > now);
        match (active, self.reason) {
            (true, Some(FloorReason::RateLimit)) => Some(RATE_DEFERRED_NOTICE),
            (true, Some(FloorReason::PollInterval)) => Some(POLL_DEFERRED_NOTICE),
            _ => None,
        }
    }
}

#[derive(Default)]
struct AccountState {
    generation: u64,
    active_generation: Option<u64>,
    cache: GeneralReadCache,
    floor: ServerFloor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReadToken {
    lifetime: u64,
    account_key: String,
    generation: u64,
}

pub(super) struct ReadAdmission {
    token: ReadToken,
    cache: GeneralReadCache,
}

impl ReadAdmission {
    pub(super) fn into_parts(self) -> (ReadToken, GeneralReadCache) {
        (self.token, self.cache)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadDeferral {
    Busy,
    Server(&'static str),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CompletionDisposition {
    pub(super) matching_operation_released: bool,
    pub(super) payload_accepted: bool,
}

pub(super) struct GeneralReadController {
    lifetime: u64,
    accounts: BTreeMap<String, AccountState>,
    refreshes: BTreeMap<RefreshLane, RefreshState>,
    followup_cursor: usize,
}

impl Default for GeneralReadController {
    fn default() -> Self {
        Self {
            lifetime: NEXT_LIFETIME.fetch_add(1, Ordering::Relaxed),
            accounts: BTreeMap::new(),
            refreshes: BTreeMap::new(),
            followup_cursor: 0,
        }
    }
}

impl GeneralReadController {
    /// Request one observation. Polls coalesce into the active read; explicit
    /// requests queue one replacement and make the older payload inadmissible.
    /// Busy/server deferrals retain the intent without occupying a lane.
    pub(super) fn request_refresh(
        &mut self,
        lane: RefreshLane,
        account: &Account,
        context: RefreshContext,
        explicit: bool,
    ) -> Result<Option<RefreshAdmission>, ReadDeferral> {
        self.request_refresh_at(lane, account, context, explicit, Instant::now())
    }

    fn request_refresh_at(
        &mut self,
        lane: RefreshLane,
        account: &Account,
        context: RefreshContext,
        explicit: bool,
        now: Instant,
    ) -> Result<Option<RefreshAdmission>, ReadDeferral> {
        if let Some(RefreshState::Active { explicit_pending }) = self.refreshes.get_mut(&lane) {
            *explicit_pending |= explicit;
            return Ok(None);
        }
        let explicit = explicit
            || matches!(
                self.refreshes.get(&lane),
                Some(RefreshState::Deferred { explicit: true })
            );
        match self.begin_at(account, now) {
            Ok(admission) => {
                self.refreshes.insert(
                    lane,
                    RefreshState::Active {
                        explicit_pending: false,
                    },
                );
                let (read, cache) = admission.into_parts();
                Ok(Some(RefreshAdmission {
                    token: RefreshToken {
                        lane,
                        context,
                        read,
                    },
                    cache,
                }))
            }
            Err(deferral) => {
                self.refreshes
                    .insert(lane, RefreshState::Deferred { explicit });
                Err(deferral)
            }
        }
    }

    /// Accept a result only for its current destination and observation. A
    /// queued explicit refresh rejects both success and failure from the old
    /// read. Server directives are retained independently of that decision.
    pub(super) fn complete_refresh<T, E>(
        &mut self,
        token: RefreshToken,
        current: Option<&RefreshContext>,
        result: Result<T, E>,
        cache: Option<GeneralReadCache>,
        directive: &GeneralReadDirective,
    ) -> RefreshCompletion<T, E> {
        self.complete_refresh_at(
            token,
            current,
            result,
            cache,
            directive,
            (Instant::now(), SystemTime::now()),
        )
    }

    fn complete_refresh_at<T, E>(
        &mut self,
        token: RefreshToken,
        current: Option<&RefreshContext>,
        result: Result<T, E>,
        cache: Option<GeneralReadCache>,
        directive: &GeneralReadDirective,
        clock: (Instant, SystemTime),
    ) -> RefreshCompletion<T, E> {
        let current = current == Some(&token.context);
        let accepted = current
            && self.refreshes.get(&token.lane).is_some_and(|state| {
                matches!(
                    state,
                    RefreshState::Active {
                        explicit_pending: false
                    }
                )
            });
        let disposition = self.complete_at(
            &token.read,
            directive,
            cache,
            accepted && result.is_ok(),
            clock.0,
            clock.1,
        );
        if !disposition.matching_operation_released {
            return RefreshCompletion::Ignored;
        }
        match self.refreshes.get_mut(&token.lane) {
            Some(
                state @ RefreshState::Active {
                    explicit_pending: true,
                },
            ) => {
                *state = RefreshState::Deferred { explicit: true };
            }
            _ => {
                self.refreshes.remove(&token.lane);
            }
        }
        if accepted {
            RefreshCompletion::Applied(result)
        } else {
            RefreshCompletion::Discarded
        }
    }

    /// Return deferred destinations in rotating order. The caller supplies all
    /// live handles, including temporarily disabled views. Forgotten handles
    /// cannot queue ghost work; their eventual completions still release the
    /// matching account slot through `complete_refresh`.
    pub(super) fn pending_refreshes(&mut self, live: &[RefreshLane]) -> Vec<RefreshLane> {
        let live_set: BTreeSet<_> = live.iter().copied().collect();
        self.refreshes.retain(|lane, _| live_set.contains(lane));
        let mut pending: Vec<_> = live
            .iter()
            .copied()
            .filter(|lane| {
                self.refreshes
                    .get(lane)
                    .is_some_and(RefreshState::has_deferred)
            })
            .collect();
        if !pending.is_empty() {
            let offset = self.followup_cursor % pending.len();
            pending.rotate_left(offset);
            self.followup_cursor = self.followup_cursor.wrapping_add(1);
        }
        pending
    }

    pub(super) fn refresh_active(&self, lane: RefreshLane) -> bool {
        self.refreshes
            .get(&lane)
            .is_some_and(|state| matches!(state, RefreshState::Active { .. }))
    }

    #[cfg(feature = "ui-smoke")]
    pub(super) fn refresh_deferred(&self, lane: RefreshLane) -> bool {
        self.refreshes
            .get(&lane)
            .is_some_and(RefreshState::has_deferred)
    }

    #[cfg(feature = "ui-smoke")]
    pub(super) fn clear_smoke_pending_after_witness(&mut self, lane: RefreshLane) {
        debug_assert!(!self.refresh_active(lane));
        self.refreshes.remove(&lane);
    }

    #[cfg(feature = "ui-smoke")]
    pub(super) fn selected_account_readiness_at(
        &self,
        account: &Account,
        now: Instant,
    ) -> Result<(), ReadDeferral> {
        let Some(state) = self.accounts.get(&account_key(account)) else {
            return Ok(());
        };
        if let Some(notice) = state.floor.notice(now) {
            return Err(ReadDeferral::Server(notice));
        }
        if state.active_generation.is_some() {
            return Err(ReadDeferral::Busy);
        }
        Ok(())
    }

    pub(super) fn begin(&mut self, account: &Account) -> Result<ReadAdmission, ReadDeferral> {
        self.begin_at(account, Instant::now())
    }

    pub(super) fn begin_at(
        &mut self,
        account: &Account,
        now: Instant,
    ) -> Result<ReadAdmission, ReadDeferral> {
        let key = account_key(account);
        let state = self.accounts.entry(key.clone()).or_default();
        if let Some(notice) = state.floor.notice(now) {
            return Err(ReadDeferral::Server(notice));
        }
        if state.active_generation.is_some() {
            return Err(ReadDeferral::Busy);
        }
        state.generation = state.generation.saturating_add(1);
        state.active_generation = Some(state.generation);
        Ok(ReadAdmission {
            token: ReadToken {
                lifetime: self.lifetime,
                account_key: key,
                generation: state.generation,
            },
            cache: state.cache.clone(),
        })
    }

    /// Install the same account floor a read would have installed, from server
    /// pacing directives disclosed by an explicit mutation.
    ///
    /// It admits nothing and releases nothing: a mutation never uses this
    /// scheduler as its authority, and a mutation never becomes a read.
    pub(super) fn apply_server_directive(
        &mut self,
        account: &Account,
        directive: &GeneralReadDirective,
    ) {
        self.apply_server_directive_at(account, directive, Instant::now(), SystemTime::now());
    }

    fn apply_server_directive_at(
        &mut self,
        account: &Account,
        directive: &GeneralReadDirective,
        now: Instant,
        wall_now: SystemTime,
    ) {
        let state = self.accounts.entry(account_key(account)).or_default();
        if let Some(delay) = &directive.rate_limit {
            state
                .floor
                .apply(delay, FloorReason::RateLimit, now, wall_now);
        }
        if let Some(delay) = &directive.x_poll_interval {
            state
                .floor
                .apply(delay, FloorReason::PollInterval, now, wall_now);
        }
    }

    pub(super) fn complete(
        &mut self,
        token: &ReadToken,
        directive: &GeneralReadDirective,
        returned_cache: Option<GeneralReadCache>,
        accept_payload: bool,
    ) -> CompletionDisposition {
        self.complete_at(
            token,
            directive,
            returned_cache,
            accept_payload,
            Instant::now(),
            SystemTime::now(),
        )
    }

    pub(super) fn complete_at(
        &mut self,
        token: &ReadToken,
        directive: &GeneralReadDirective,
        returned_cache: Option<GeneralReadCache>,
        accept_payload: bool,
        now: Instant,
        wall_now: SystemTime,
    ) -> CompletionDisposition {
        if token.lifetime != self.lifetime {
            return CompletionDisposition::default();
        }
        let Some(state) = self.accounts.get_mut(&token.account_key) else {
            return CompletionDisposition::default();
        };
        if let Some(delay) = &directive.rate_limit {
            state
                .floor
                .apply(delay, FloorReason::RateLimit, now, wall_now);
        }
        if let Some(delay) = &directive.x_poll_interval {
            state
                .floor
                .apply(delay, FloorReason::PollInterval, now, wall_now);
        }
        let matching = state.active_generation == Some(token.generation);
        if !matching {
            return CompletionDisposition::default();
        }
        if accept_payload && let Some(cache) = returned_cache {
            state.cache = cache;
        }
        state.active_generation = None;
        CompletionDisposition {
            matching_operation_released: true,
            payload_accepted: accept_payload,
        }
    }

    #[cfg(test)]
    fn notice_at(&self, account: &Account, now: Instant) -> Option<&'static str> {
        self.accounts
            .get(&account_key(account))
            .and_then(|state| state.floor.notice(now))
    }
}

fn account_key(account: &Account) -> String {
    format!(
        "github\u{1f}{}\u{1f}{}",
        account.host.to_ascii_lowercase(),
        account.login.to_ascii_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(login: &str) -> Account {
        Account {
            host: "github.com".into(),
            login: login.into(),
        }
    }

    #[test]
    fn rate_floor_is_account_local_monotonic_and_survives_success() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(10_000);
        let alice = account("alice");
        let bob = account("bob");
        let mut controller = GeneralReadController::default();
        let (token, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        let rate = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(120)),
        };
        assert!(
            controller
                .complete_at(&token, &rate, None, false, now, wall)
                .matching_operation_released
        );
        assert!(matches!(
            controller.begin_at(&alice, now + Duration::from_secs(119)),
            Err(ReadDeferral::Server(RATE_DEFERRED_NOTICE))
        ));
        assert!(controller.begin_at(&bob, now).is_ok());

        let later = now + Duration::from_secs(120);
        let (token, _) = controller.begin_at(&alice, later).unwrap().into_parts();
        let success = GeneralReadDirective::default();
        controller.complete_at(&token, &success, None, true, later, wall);
        assert!(controller.begin_at(&alice, later).is_ok());
    }

    #[test]
    fn mutation_directive_installs_an_account_floor_without_admitting_a_read() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(10_000);
        let alice = account("alice");
        let bob = account("bob");
        let mut controller = GeneralReadController::default();
        let rate = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(90)),
        };
        controller.apply_server_directive_at(&alice, &rate, now, wall);
        assert!(matches!(
            controller.begin_at(&alice, now + Duration::from_secs(89)),
            Err(ReadDeferral::Server(RATE_DEFERRED_NOTICE))
        ));
        let (other, _) = controller.begin_at(&bob, now).unwrap().into_parts();
        let success = GeneralReadDirective::default();
        assert!(
            controller
                .complete_at(&other, &success, None, true, now, wall)
                .matching_operation_released
        );

        // The mutation never occupied the account, so the floor alone gates it.
        let later = now + Duration::from_secs(90);
        let (released, _) = controller.begin_at(&alice, later).unwrap().into_parts();
        assert!(
            controller
                .complete_at(&released, &success, None, true, later, wall)
                .matching_operation_released
        );

        // A shorter later directive can never shorten an installed floor.
        let (token, _) = controller.begin_at(&bob, later).unwrap().into_parts();
        let long = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(600)),
        };
        controller.complete_at(&token, &long, None, true, later, wall);
        let short = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(5)),
        };
        controller.apply_server_directive_at(&bob, &short, later, wall);
        assert!(matches!(
            controller.begin_at(&bob, later + Duration::from_secs(599)),
            Err(ReadDeferral::Server(RATE_DEFERRED_NOTICE))
        ));
    }

    #[test]
    fn a_stale_read_completion_cannot_erase_a_mutation_installed_floor() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(10_000);
        let alice = account("alice");
        let mut controller = GeneralReadController::default();
        let (stale, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        let success = GeneralReadDirective::default();
        controller.complete_at(&stale, &success, None, true, now, wall);

        let rate = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(300)),
        };
        controller.apply_server_directive_at(&alice, &rate, now, wall);

        // A completion from an already-released generation, and one from a
        // foreign controller lifetime, both leave the floor intact.
        controller.complete_at(&stale, &success, None, true, now, wall);
        let foreign = ReadToken {
            lifetime: stale.lifetime.wrapping_add(1),
            account_key: stale.account_key.clone(),
            generation: stale.generation,
        };
        controller.complete_at(&foreign, &success, None, true, now, wall);
        assert!(matches!(
            controller.begin_at(&alice, now + Duration::from_secs(299)),
            Err(ReadDeferral::Server(RATE_DEFERRED_NOTICE))
        ));
        assert!(
            controller
                .begin_at(&alice, now + Duration::from_secs(300))
                .is_ok()
        );
    }

    #[test]
    fn stale_generation_can_only_extend_matching_account_floor() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(10_000);
        let alice = account("alice");
        let mut controller = GeneralReadController::default();
        let (old, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        controller.complete_at(
            &old,
            &GeneralReadDirective::default(),
            None,
            false,
            now,
            wall,
        );
        let (current, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        let stale_rate = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(60)),
        };
        let disposition = controller.complete_at(
            &old,
            &stale_rate,
            Some(GeneralReadCache::default()),
            true,
            now,
            wall,
        );
        assert_eq!(disposition, CompletionDisposition::default());
        assert_eq!(
            controller.notice_at(&alice, now),
            Some(RATE_DEFERRED_NOTICE)
        );
        assert!(matches!(
            controller.begin_at(&alice, now),
            Err(ReadDeferral::Server(RATE_DEFERRED_NOTICE))
        ));
        assert!(
            controller
                .complete_at(
                    &current,
                    &GeneralReadDirective::default(),
                    None,
                    false,
                    now,
                    wall,
                )
                .matching_operation_released
        );
    }

    #[test]
    fn stale_lifetime_cannot_install_floor_or_release_current_work() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(10_000);
        let alice = account("alice");
        let mut old_controller = GeneralReadController::default();
        let (old, _) = old_controller.begin_at(&alice, now).unwrap().into_parts();
        let mut current = GeneralReadController::default();
        let (current_token, _) = current.begin_at(&alice, now).unwrap().into_parts();
        let disposition = current.complete_at(
            &old,
            &GeneralReadDirective {
                x_poll_interval: None,
                rate_limit: Some(GeneralReadDelay::Suspend),
            },
            Some(GeneralReadCache::default()),
            true,
            now,
            wall,
        );
        assert_eq!(disposition, CompletionDisposition::default());
        assert_eq!(current.notice_at(&alice, now), None);
        assert!(
            current
                .complete_at(
                    &current_token,
                    &GeneralReadDirective::default(),
                    None,
                    false,
                    now,
                    wall,
                )
                .matching_operation_released
        );
    }

    #[test]
    fn expired_reset_does_not_suspend_or_shorten_a_later_floor() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(20_000);
        let alice = account("alice");
        let mut controller = GeneralReadController::default();
        let (stale, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        controller.complete_at(
            &stale,
            &GeneralReadDirective::default(),
            None,
            false,
            now,
            wall,
        );
        let (current, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        controller.complete_at(
            &stale,
            &GeneralReadDirective {
                x_poll_interval: None,
                rate_limit: Some(GeneralReadDelay::Seconds(90)),
            },
            None,
            false,
            now,
            wall,
        );
        controller.complete_at(
            &current,
            &GeneralReadDirective {
                x_poll_interval: None,
                rate_limit: Some(GeneralReadDelay::UntilUnixSeconds(19_999)),
            },
            None,
            false,
            now + Duration::from_secs(10),
            wall,
        );
        assert_eq!(
            controller.notice_at(&alice, now + Duration::from_secs(89)),
            Some(RATE_DEFERRED_NOTICE)
        );
        assert!(
            controller
                .begin_at(&alice, now + Duration::from_secs(90))
                .is_ok()
        );
    }

    #[test]
    fn poll_floor_survives_rejected_payload_and_explicit_follow_up() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(30_000);
        let alice = account("alice");
        let directive = GeneralReadDirective {
            x_poll_interval: Some(GeneralReadDelay::Seconds(90)),
            rate_limit: None,
        };

        let mut controller = GeneralReadController::default();
        let (token, _) = controller.begin_at(&alice, now).unwrap().into_parts();
        let disposition = controller.complete_at(
            &token,
            &directive,
            Some(GeneralReadCache::default()),
            false,
            now,
            wall,
        );
        assert!(disposition.matching_operation_released);
        assert!(!disposition.payload_accepted);
        assert!(matches!(
            controller.begin_at(&alice, now + Duration::from_secs(89)),
            Err(ReadDeferral::Server(POLL_DEFERRED_NOTICE))
        ));
        assert!(
            controller
                .begin_at(&alice, now + Duration::from_secs(90))
                .is_ok()
        );
    }
    fn refresh_context() -> RefreshContext {
        RefreshContext {
            workspace: 7,
            repository: "github.com/alice/octo/repo".into(),
            resource: Some(42),
            revision: 1,
        }
    }

    #[test]
    fn slow_refresh_accepts_its_result_despite_repeated_polls() {
        let mut reads = GeneralReadController::default();
        let lane = RefreshLane::default();
        let account = account("alice");
        let context = refresh_context();
        let (token, _) = reads
            .request_refresh(lane, &account, context.clone(), false)
            .unwrap()
            .unwrap()
            .into_parts();
        for _ in 0..20 {
            assert!(
                reads
                    .request_refresh(lane, &account, context.clone(), false)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            reads.complete_refresh(
                token,
                Some(&context),
                Ok::<_, ()>("observed"),
                None,
                &GeneralReadDirective::default()
            ),
            RefreshCompletion::Applied(Ok("observed"))
        );
        assert!(reads.pending_refreshes(&[lane]).is_empty());
        assert!(
            reads
                .request_refresh(lane, &account, context, false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn explicit_requests_coalesce_and_reject_both_old_success_and_failure() {
        for result in [Ok("old observation"), Err("old failure")] {
            let mut reads = GeneralReadController::default();
            let lane = RefreshLane::default();
            let account = account("alice");
            let context = refresh_context();
            let (token, _) = reads
                .request_refresh(lane, &account, context.clone(), false)
                .unwrap()
                .unwrap()
                .into_parts();
            for _ in 0..10 {
                assert!(
                    reads
                        .request_refresh(lane, &account, context.clone(), true)
                        .unwrap()
                        .is_none()
                );
                assert!(
                    reads
                        .request_refresh(lane, &account, context.clone(), false)
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(
                reads.complete_refresh(
                    token,
                    Some(&context),
                    result,
                    None,
                    &GeneralReadDirective::default()
                ),
                RefreshCompletion::Discarded
            );
            assert_eq!(reads.pending_refreshes(&[lane]), vec![lane]);
            let (next, _) = reads
                .request_refresh(lane, &account, context.clone(), false)
                .unwrap()
                .unwrap()
                .into_parts();
            assert_eq!(
                reads.complete_refresh(
                    next,
                    Some(&context),
                    Ok::<_, ()>("fresh"),
                    None,
                    &GeneralReadDirective::default()
                ),
                RefreshCompletion::Applied(Ok("fresh"))
            );
            assert!(reads.pending_refreshes(&[lane]).is_empty());
        }
    }

    #[test]
    fn deferred_explicit_followup_obeys_server_floor_without_stranding_lane() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(50_000);
        let mut reads = GeneralReadController::default();
        let lane = RefreshLane::default();
        let account = account("alice");
        let context = refresh_context();
        let (token, _) = reads
            .request_refresh_at(lane, &account, context.clone(), false, now)
            .unwrap()
            .unwrap()
            .into_parts();
        assert!(
            reads
                .request_refresh_at(lane, &account, context.clone(), true, now)
                .unwrap()
                .is_none()
        );
        let directive = GeneralReadDirective {
            x_poll_interval: Some(GeneralReadDelay::Seconds(90)),
            rate_limit: None,
        };
        assert_eq!(
            reads.complete_refresh_at(
                token,
                Some(&context),
                Ok::<_, ()>(1),
                None,
                &directive,
                (now, wall)
            ),
            RefreshCompletion::Discarded
        );
        assert!(matches!(
            reads.request_refresh_at(
                lane,
                &account,
                context.clone(),
                false,
                now + Duration::from_secs(89)
            ),
            Err(ReadDeferral::Server(POLL_DEFERRED_NOTICE))
        ));
        assert_eq!(reads.pending_refreshes(&[lane]), vec![lane]);
        let (next, _) = reads
            .request_refresh_at(
                lane,
                &account,
                context.clone(),
                false,
                now + Duration::from_secs(90),
            )
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(
            reads.complete_refresh_at(
                next,
                Some(&context),
                Ok::<_, ()>(2),
                None,
                &GeneralReadDirective::default(),
                (now + Duration::from_secs(90), wall)
            ),
            RefreshCompletion::Applied(Ok(2))
        );
        assert!(reads.pending_refreshes(&[lane]).is_empty());
    }

    #[test]
    fn stale_refresh_identity_rejects_success_and_error_but_releases_account() {
        let expected = refresh_context();
        let mut changed_workspace = expected.clone();
        changed_workspace.workspace += 1;
        let mut changed_account = expected.clone();
        changed_account.repository = "github.com/bob/octo/repo".into();
        let mut changed_pr = expected.clone();
        changed_pr.resource = Some(43);
        let mut invalidated = expected.clone();
        invalidated.revision += 1;
        for current in [changed_workspace, changed_account, changed_pr, invalidated] {
            for result in [Ok("old"), Err("old error")] {
                let mut reads = GeneralReadController::default();
                let lane = RefreshLane::default();
                let account = account("alice");
                let (token, _) = reads
                    .request_refresh(lane, &account, expected.clone(), false)
                    .unwrap()
                    .unwrap()
                    .into_parts();
                assert_eq!(
                    reads.complete_refresh(
                        token,
                        Some(&current),
                        result,
                        None,
                        &GeneralReadDirective::default()
                    ),
                    RefreshCompletion::Discarded
                );
                assert!(!reads.refresh_active(lane));
                assert!(
                    reads
                        .request_refresh(lane, &account, current.clone(), true)
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn mutation_invalidates_observation_without_losing_queued_refresh_or_rate_floor() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(50_000);
        let mut reads = GeneralReadController::default();
        let lane = RefreshLane::default();
        let account = account("alice");
        let context = refresh_context();
        let (token, _) = reads
            .request_refresh_at(lane, &account, context.clone(), false, now)
            .unwrap()
            .unwrap()
            .into_parts();
        let mut current = context;
        current.revision += 1;
        assert!(
            reads
                .request_refresh_at(lane, &account, current.clone(), true, now)
                .unwrap()
                .is_none()
        );
        let directive = GeneralReadDirective {
            x_poll_interval: None,
            rate_limit: Some(GeneralReadDelay::Seconds(30)),
        };
        assert_eq!(
            reads.complete_refresh_at(
                token,
                Some(&current),
                Ok::<_, ()>("stale"),
                None,
                &directive,
                (now, wall)
            ),
            RefreshCompletion::Discarded
        );
        assert_eq!(reads.pending_refreshes(&[lane]), vec![lane]);
        assert!(matches!(
            reads.request_refresh_at(lane, &account, current.clone(), false, now),
            Err(ReadDeferral::Server(RATE_DEFERRED_NOTICE))
        ));
        assert!(
            reads
                .request_refresh_at(
                    lane,
                    &account,
                    current,
                    false,
                    now + Duration::from_secs(30)
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn closed_and_reopened_destination_cannot_receive_old_completion() {
        let mut reads = GeneralReadController::default();
        let old_lane = RefreshLane::default();
        let reopened = RefreshLane::default();
        let account = account("alice");
        let context = refresh_context();
        let (old, _) = reads
            .request_refresh(old_lane, &account, context.clone(), false)
            .unwrap()
            .unwrap()
            .into_parts();
        assert!(
            reads
                .request_refresh(old_lane, &account, context.clone(), true)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            reads.request_refresh(reopened, &account, context.clone(), true),
            Err(ReadDeferral::Busy)
        ));
        assert_eq!(reads.pending_refreshes(&[reopened]), vec![reopened]);
        // Even identical resource and revision values do not revive a forgotten
        // lane. Closing it did not cancel the still-running account operation.
        assert_eq!(
            reads.complete_refresh(
                old,
                Some(&context),
                Ok::<_, ()>("old"),
                None,
                &GeneralReadDirective::default()
            ),
            RefreshCompletion::Discarded
        );
        assert_eq!(reads.pending_refreshes(&[reopened]), vec![reopened]);
        let (next, _) = reads
            .request_refresh(reopened, &account, context.clone(), false)
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(
            reads.complete_refresh(
                next,
                Some(&context),
                Ok::<_, ()>("new"),
                None,
                &GeneralReadDirective::default()
            ),
            RefreshCompletion::Applied(Ok("new"))
        );
    }

    #[test]
    fn independent_account_refreshes_and_direct_actions_share_admission_correctly() {
        let mut reads = GeneralReadController::default();
        let alice = account("alice");
        let bob = account("bob");
        let lane = RefreshLane::default();
        let other = RefreshLane::default();
        let context = refresh_context();
        // The direct account admission is also used by Actions jobs/logs.
        let (actions, _) = reads.begin(&alice).unwrap().into_parts();
        assert!(matches!(
            reads.request_refresh(lane, &alice, context.clone(), true),
            Err(ReadDeferral::Busy)
        ));
        let (bob_read, _) = reads
            .request_refresh(other, &bob, context.clone(), true)
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(
            reads.complete_refresh(
                bob_read,
                Some(&context),
                Err::<(), _>("unavailable"),
                None,
                &GeneralReadDirective::default()
            ),
            RefreshCompletion::Applied(Err("unavailable"))
        );
        assert_eq!(reads.pending_refreshes(&[lane, other]), vec![lane]);
        assert!(
            reads
                .complete(&actions, &GeneralReadDirective::default(), None, true)
                .matching_operation_released
        );
        assert!(
            reads
                .request_refresh(lane, &alice, context, false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn periodic_ticks_do_not_starve_deferred_same_account_destinations() {
        let mut reads = GeneralReadController::default();
        let account = account("alice");
        let context = refresh_context();
        // Metadata, details, lifecycle and a later sidebar repository.
        let lanes: [RefreshLane; 4] = std::array::from_fn(|_| Default::default());
        let mut installed = [0; 4];
        let mut active = None;
        for (index, lane) in lanes.iter().enumerate() {
            match reads.request_refresh(*lane, &account, context.clone(), false) {
                Ok(Some(admission)) => active = Some((index, admission.into_parts().0)),
                Err(ReadDeferral::Busy) => {}
                _ => panic!("unexpected admission"),
            }
        }
        for _ in 0..16 {
            let (index, token) = active.take().unwrap();
            // Repeated periodic requests do not supersede this slow result.
            for lane in lanes {
                assert!(!matches!(
                    reads.request_refresh(lane, &account, context.clone(), false),
                    Ok(Some(_))
                ));
            }
            assert_eq!(
                reads.complete_refresh(
                    token,
                    Some(&context),
                    Ok::<_, ()>(index),
                    None,
                    &GeneralReadDirective::default()
                ),
                RefreshCompletion::Applied(Ok(index))
            );
            installed[index] += 1;
            if installed.iter().all(|count| *count > 0) {
                break;
            }
            for lane in reads.pending_refreshes(&lanes) {
                match reads.request_refresh(lane, &account, context.clone(), false) {
                    Ok(Some(admission)) => {
                        let index = lanes
                            .iter()
                            .position(|candidate| *candidate == lane)
                            .unwrap();
                        active = Some((index, admission.into_parts().0));
                    }
                    Err(ReadDeferral::Busy) => {}
                    _ => panic!("deferred lane must be ready or account busy"),
                }
            }
        }
        assert_eq!(installed, [1; 4]);
    }

    #[test]
    fn replaced_controller_rejects_old_refresh_without_touching_current_admission() {
        let account = account("alice");
        let context = refresh_context();
        let lane = RefreshLane::default();
        let mut old_reads = GeneralReadController::default();
        let (old, _) = old_reads
            .request_refresh(lane, &account, context.clone(), false)
            .unwrap()
            .unwrap()
            .into_parts();
        let mut reads = GeneralReadController::default();
        let (current, _) = reads
            .request_refresh(lane, &account, context.clone(), false)
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(
            reads.complete_refresh(
                old,
                Some(&context),
                Ok::<_, ()>("old"),
                None,
                &GeneralReadDirective {
                    rate_limit: Some(GeneralReadDelay::Suspend),
                    x_poll_interval: None
                }
            ),
            RefreshCompletion::Ignored
        );
        assert!(reads.refresh_active(lane));
        assert_eq!(
            reads.complete_refresh(
                current,
                Some(&context),
                Ok::<_, ()>("new"),
                None,
                &GeneralReadDirective::default()
            ),
            RefreshCompletion::Applied(Ok("new"))
        );
        assert!(
            reads
                .request_refresh(lane, &account, context, false)
                .unwrap()
                .is_some()
        );
    }
}
