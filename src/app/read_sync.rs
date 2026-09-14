use cibergit::{
    domain::Account,
    providers::{GeneralReadCache, GeneralReadDelay, GeneralReadDirective},
};
use std::{
    collections::BTreeMap,
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
}

impl Default for GeneralReadController {
    fn default() -> Self {
        Self {
            lifetime: NEXT_LIFETIME.fetch_add(1, Ordering::Relaxed),
            accounts: BTreeMap::new(),
        }
    }
}

impl GeneralReadController {
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
}
