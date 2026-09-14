//! Bounded parser and metadata for `gh api --include` notification reads.
//! Diagnostics deliberately contain no remote body, stderr, token, or header values.

use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    fmt,
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_FIELDS: usize = 128;
const MAX_ETAG_BYTES: usize = 1024;
const MAX_LAST_MODIFIED_BYTES: usize = 128;
const MAX_LINK_BYTES: usize = 16 * 1024;
const MAX_DIRECTIVE_BYTES: usize = 32;
const MAX_DECIMAL_DIGITS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneralReadFailureKind {
    Unavailable,
    RateLimited,
    Incomplete,
}

#[derive(Default)]
struct GeneralReadTrackerState {
    poll: RestPollDirective,
    failure: Option<GeneralReadFailureKind>,
    halted: bool,
}

#[derive(Clone)]
pub(crate) struct GeneralReadTracker(Rc<RefCell<GeneralReadTrackerState>>);

thread_local! {
    static GENERAL_READ_STACK: RefCell<Vec<GeneralReadTracker>> = const { RefCell::new(Vec::new()) };
}

impl GeneralReadTracker {
    pub(crate) fn new() -> Self {
        Self(Rc::new(RefCell::new(GeneralReadTrackerState::default())))
    }

    pub(crate) fn record_poll(&self, poll: &RestPollDirective) {
        let mut state = self.0.borrow_mut();
        state.poll.merge(poll);
        if poll.rate_limit.is_some() {
            state.failure = Some(GeneralReadFailureKind::RateLimited);
            state.halted = true;
        }
    }

    pub(crate) fn record_failure(&self, kind: GeneralReadFailureKind) {
        let mut state = self.0.borrow_mut();
        if state.failure != Some(GeneralReadFailureKind::RateLimited) {
            state.failure = Some(kind);
        }
    }

    pub(crate) fn record_error(&self, error: &RestReadError) {
        self.record_poll(error.poll());
        if error.poll().rate_limit.is_none() {
            self.record_failure(error.general_failure_kind());
        }
    }

    pub(crate) fn check_not_halted(&self) -> Result<(), RestReadError> {
        let state = self.0.borrow();
        if state.halted {
            Err(RestReadError::new(
                RestReadErrorKind::RateDeferred,
                state.poll.clone(),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn snapshot(&self) -> (RestPollDirective, Option<GeneralReadFailureKind>) {
        let state = self.0.borrow();
        (state.poll.clone(), state.failure)
    }
}

pub(crate) fn with_general_read_tracker<T>(
    operation: impl FnOnce() -> T,
) -> (T, RestPollDirective, Option<GeneralReadFailureKind>) {
    struct PopTracker;
    impl Drop for PopTracker {
        fn drop(&mut self) {
            GENERAL_READ_STACK.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }

    let tracker = GeneralReadTracker::new();
    GENERAL_READ_STACK.with(|stack| stack.borrow_mut().push(tracker.clone()));
    let guard = PopTracker;
    let result = operation();
    drop(guard);
    let (poll, failure) = tracker.snapshot();
    (result, poll, failure)
}

pub(crate) fn active_general_read_tracker() -> Option<GeneralReadTracker> {
    GENERAL_READ_STACK.with(|stack| stack.borrow().last().cloned())
}

pub(crate) fn record_general_poll_from_included_prefix(prefix: &[u8]) {
    let poll = general_poll_from_included_prefix(prefix);
    if let Some(tracker) = active_general_read_tracker() {
        tracker.record_poll(&poll);
    }
}

pub(crate) fn general_poll_from_included_prefix(prefix: &[u8]) -> RestPollDirective {
    let bounded = &prefix[..prefix.len().min(MAX_HEADER_BYTES)];
    if let Some((header_end, _)) = find_header_end(bounded) {
        let Ok(collected) = collect_header_fields(&bounded[..header_end]) else {
            return RestPollDirective::default();
        };
        let (poll, _, graphql_rate_limit) = parse_poll_directive(
            collected.status,
            &collected.fields,
            collected.error.is_some(),
        );
        return promote_graphql_rate_on_error(poll, false, graphql_rate_limit);
    }
    rejected_header_poll(bounded, false)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestValidators {
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
}

impl RestValidators {
    pub(crate) fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    pub(crate) fn request_header(&self) -> Option<(&'static str, &str)> {
        self.etag
            .as_deref()
            .map(|value| ("If-None-Match", value))
            .or_else(|| {
                self.last_modified
                    .as_deref()
                    .map(|value| ("If-Modified-Since", value))
            })
    }

    pub(crate) fn merged_after_not_modified(&self, returned: &Self) -> Self {
        Self {
            etag: returned.etag.clone().or_else(|| self.etag.clone()),
            last_modified: returned
                .last_modified
                .clone()
                .or_else(|| self.last_modified.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundedDelay {
    Seconds(u64),
    UntilUnixSeconds(u64),
    Suspend,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestPollDirective {
    pub x_poll_interval: Option<BoundedDelay>,
    pub rate_limit: Option<BoundedDelay>,
}

impl RestPollDirective {
    pub fn merge(&mut self, other: &Self) {
        self.x_poll_interval =
            merge_delay(self.x_poll_interval.take(), other.x_poll_interval.clone());
        self.rate_limit = merge_delay(self.rate_limit.take(), other.rate_limit.clone());
    }
}

fn merge_delay(left: Option<BoundedDelay>, right: Option<BoundedDelay>) -> Option<BoundedDelay> {
    use BoundedDelay::*;
    match (left, right) {
        (None, value) | (value, None) => value,
        (Some(Suspend), _) | (_, Some(Suspend)) => Some(Suspend),
        (Some(Seconds(a)), Some(Seconds(b))) => Some(Seconds(a.max(b))),
        (Some(UntilUnixSeconds(a)), Some(UntilUnixSeconds(b))) => Some(UntilUnixSeconds(a.max(b))),
        (Some(Seconds(seconds)), Some(UntilUnixSeconds(until)))
        | (Some(UntilUnixSeconds(until)), Some(Seconds(seconds))) => {
            let Some(now) = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|value| value.as_secs())
            else {
                return Some(Suspend);
            };
            Some(
                now.checked_add(seconds)
                    .map_or(Suspend, |deadline| UntilUnixSeconds(deadline.max(until))),
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestResponseMetadata {
    pub(crate) validators: RestValidators,
    pub(crate) link: Option<String>,
    pub(crate) poll: RestPollDirective,
    pub(crate) graphql_rate_limit: Option<BoundedDelay>,
    pub(crate) body_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConditionalGet<T> {
    Modified {
        value: T,
        metadata: RestResponseMetadata,
    },
    NotModified {
        metadata: RestResponseMetadata,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestReadErrorKind {
    Credential,
    Cancelled,
    TimedOut,
    Transport,
    OperationLimit,
    InvalidFraming,
    InvalidHeaders,
    InvalidBody,
    HttpFailure,
    RateDeferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestReadErrorClass {
    Credential,
    Cancelled,
    TimedOut,
    OperationLimit,
    InvalidResponse,
    Transport,
    HttpFailure,
    RateDeferred,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestReadError {
    kind: RestReadErrorKind,
    poll: RestPollDirective,
    status: Option<u16>,
}

impl RestReadError {
    fn new(kind: RestReadErrorKind, poll: RestPollDirective) -> Self {
        Self {
            kind,
            poll,
            status: None,
        }
    }
    pub(crate) fn credential() -> Self {
        Self::new(RestReadErrorKind::Credential, RestPollDirective::default())
    }
    pub(crate) fn cancelled() -> Self {
        Self::new(RestReadErrorKind::Cancelled, RestPollDirective::default())
    }
    pub(crate) fn timed_out() -> Self {
        Self::new(RestReadErrorKind::TimedOut, RestPollDirective::default())
    }
    pub(crate) fn transport() -> Self {
        Self::new(RestReadErrorKind::Transport, RestPollDirective::default())
    }
    pub(crate) fn operation_limit() -> Self {
        Self::new(
            RestReadErrorKind::OperationLimit,
            RestPollDirective::default(),
        )
    }
    pub(crate) fn invalid_body(poll: RestPollDirective) -> Self {
        Self::new(RestReadErrorKind::InvalidBody, poll)
    }
    pub(crate) fn poll(&self) -> &RestPollDirective {
        &self.poll
    }
    pub(crate) fn http_status(&self) -> Option<u16> {
        self.status
    }
    pub(crate) fn class(&self) -> RestReadErrorClass {
        match self.kind {
            RestReadErrorKind::Credential => RestReadErrorClass::Credential,
            RestReadErrorKind::Cancelled => RestReadErrorClass::Cancelled,
            RestReadErrorKind::TimedOut => RestReadErrorClass::TimedOut,
            RestReadErrorKind::OperationLimit => RestReadErrorClass::OperationLimit,
            RestReadErrorKind::InvalidFraming
            | RestReadErrorKind::InvalidHeaders
            | RestReadErrorKind::InvalidBody => RestReadErrorClass::InvalidResponse,
            RestReadErrorKind::Transport => RestReadErrorClass::Transport,
            RestReadErrorKind::HttpFailure => RestReadErrorClass::HttpFailure,
            RestReadErrorKind::RateDeferred => RestReadErrorClass::RateDeferred,
        }
    }
    pub(crate) fn invalidates_cached_body(&self) -> bool {
        matches!(self.kind, RestReadErrorKind::InvalidBody)
    }
    fn general_failure_kind(&self) -> GeneralReadFailureKind {
        if self.poll.rate_limit.is_some() || matches!(self.kind, RestReadErrorKind::RateDeferred) {
            GeneralReadFailureKind::RateLimited
        } else if matches!(
            self.kind,
            RestReadErrorKind::Credential
                | RestReadErrorKind::Cancelled
                | RestReadErrorKind::TimedOut
                | RestReadErrorKind::Transport
        ) {
            GeneralReadFailureKind::Unavailable
        } else {
            GeneralReadFailureKind::Incomplete
        }
    }
    fn with_poll(mut self, poll: RestPollDirective) -> Self {
        self.poll = poll;
        self
    }
    fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }
}

impl fmt::Display for RestReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            RestReadErrorKind::Credential => "Selected GitHub credential is unavailable",
            RestReadErrorKind::Cancelled => "GitHub conditional read was cancelled",
            RestReadErrorKind::TimedOut => "GitHub conditional read timed out",
            RestReadErrorKind::Transport => "GitHub conditional read transport failed",
            RestReadErrorKind::OperationLimit => "GitHub conditional read operation limit reached",
            RestReadErrorKind::InvalidFraming => "Invalid GitHub included-response framing",
            RestReadErrorKind::InvalidHeaders => "Invalid GitHub response metadata",
            RestReadErrorKind::InvalidBody => "Invalid or incomplete GitHub JSON response",
            RestReadErrorKind::HttpFailure => "GitHub conditional read was rejected",
            RestReadErrorKind::RateDeferred => {
                "GitHub read was stopped by a server rate-limit response"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RestReadError {}

pub(crate) fn parse_included_response(
    output: &[u8],
    process_success: bool,
) -> Result<ConditionalGet<Vec<u8>>, RestReadError> {
    parse_included_response_for(output, process_success, false)
}

pub(crate) fn parse_graphql_included_response(
    output: &[u8],
    process_success: bool,
) -> Result<ConditionalGet<Vec<u8>>, RestReadError> {
    parse_included_response_for(output, process_success, true)
}

fn parse_included_response_for(
    output: &[u8],
    process_success: bool,
    graphql: bool,
) -> Result<ConditionalGet<Vec<u8>>, RestReadError> {
    let Some((header_end, delimiter_len)) = find_header_end(output) else {
        let poll = rejected_header_poll(&output[..output.len().min(MAX_HEADER_BYTES)], graphql);
        return Err(RestReadError::new(RestReadErrorKind::InvalidFraming, poll));
    };
    if header_end > MAX_HEADER_BYTES {
        let poll = rejected_header_poll(&output[..MAX_HEADER_BYTES], graphql);
        return Err(RestReadError::new(RestReadErrorKind::InvalidHeaders, poll));
    }
    let header = &output[..header_end];
    let collected = collect_header_fields(header)?;
    let (poll, directive_error, graphql_rate_limit) = parse_poll_directive(
        collected.status,
        &collected.fields,
        collected.error.is_some(),
    );
    let error_poll =
        promote_graphql_rate_on_error(poll.clone(), graphql, graphql_rate_limit.clone());
    if let Some(error) = collected.error.or(directive_error) {
        return Err(error.with_poll(error_poll));
    }
    let validators =
        parse_validators(&collected.fields).map_err(|error| error.with_poll(error_poll.clone()))?;
    let link = single(&collected.fields, "link")
        .and_then(|value| {
            value
                .map(|value| validate_visible(value, MAX_LINK_BYTES))
                .transpose()
        })
        .map_err(|error| error.with_poll(error_poll.clone()))?
        .map(str::to_owned);
    let body = &output[header_end + delimiter_len..];
    let metadata = RestResponseMetadata {
        validators,
        link,
        poll: poll.clone(),
        graphql_rate_limit,
        body_bytes: body.len(),
    };
    match collected.status {
        200 if process_success && !body.is_empty() => Ok(ConditionalGet::Modified {
            value: body.to_vec(),
            metadata,
        }),
        304 if body.is_empty() => Ok(ConditionalGet::NotModified { metadata }),
        200 | 304 => Err(RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            error_poll,
        )),
        _ => Err(
            RestReadError::new(RestReadErrorKind::HttpFailure, error_poll)
                .with_status(collected.status),
        ),
    }
}

struct CollectedHeaderFields {
    status: u16,
    fields: Vec<(String, String)>,
    error: Option<RestReadError>,
}

fn collect_header_fields(header: &[u8]) -> Result<CollectedHeaderFields, RestReadError> {
    let mut lines = header.split(|byte| *byte == b'\n');
    let status_bytes = lines.next().ok_or_else(|| {
        RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        )
    })?;
    let status_bytes = status_bytes.strip_suffix(b"\r").unwrap_or(status_bytes);
    if status_bytes.contains(&0) || status_bytes.contains(&b'\r') {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        ));
    }
    let status_text = std::str::from_utf8(status_bytes).map_err(|_| {
        RestReadError::new(
            RestReadErrorKind::InvalidHeaders,
            RestPollDirective::default(),
        )
    })?;
    let status = parse_status(status_text)?;
    let mut fields = Vec::new();
    let mut error = None;
    for raw_line in lines {
        let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if raw_line.contains(&0) || raw_line.contains(&b'\r') {
            error = Some(RestReadError::new(
                RestReadErrorKind::InvalidFraming,
                RestPollDirective::default(),
            ));
            break;
        }
        let Ok(line) = std::str::from_utf8(raw_line) else {
            error = Some(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
            break;
        };
        if fields.len() == MAX_HEADER_FIELDS
            || line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with("HTTP/")
        {
            error = Some(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            error = Some(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
            break;
        };
        if name.is_empty() || !name.bytes().all(is_token) || value.bytes().any(|byte| byte == b'\n')
        {
            error = Some(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
            break;
        }
        fields.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(CollectedHeaderFields {
        status,
        fields,
        error,
    })
}

fn rejected_header_poll(prefix: &[u8], graphql: bool) -> RestPollDirective {
    let Some(last_newline) = prefix.iter().rposition(|byte| *byte == b'\n') else {
        let status = std::str::from_utf8(prefix)
            .ok()
            .and_then(|line| parse_status(line).ok());
        return RestPollDirective {
            x_poll_interval: None,
            rate_limit: status
                .filter(|status| matches!(status, 403 | 429))
                .map(|_| BoundedDelay::Suspend),
        };
    };
    let complete = &prefix[..last_newline];
    let partial = &prefix[last_newline + 1..];
    let Ok(collected) = collect_header_fields(complete) else {
        return RestPollDirective::default();
    };
    let (mut poll, _, graphql_rate_limit) =
        parse_poll_directive(collected.status, &collected.fields, true);
    poll = promote_graphql_rate_on_error(poll, graphql, graphql_rate_limit);
    let partial_name = partial
        .iter()
        .position(|byte| *byte == b':')
        .map(|colon| &partial[..colon]);
    if partial_name.is_some_and(|name| name.eq_ignore_ascii_case(b"x-poll-interval")) {
        poll.x_poll_interval = Some(BoundedDelay::Suspend);
    }
    let partial_retry = partial_name.is_some_and(|name| name.eq_ignore_ascii_case(b"retry-after"));
    let partial_rate = partial_retry
        || partial_name.is_some_and(|name| {
            name.eq_ignore_ascii_case(b"x-ratelimit-remaining")
                || name.eq_ignore_ascii_case(b"x-ratelimit-reset")
        });
    let complete_retry = capture_decimal(&collected.fields, "retry-after").0;
    if matches!(collected.status, 403 | 429)
        && partial_rate
        && (partial_retry || bounded_delay(&complete_retry).is_none())
    {
        poll.rate_limit = Some(BoundedDelay::Suspend);
    }
    poll
}

fn promote_graphql_rate_on_error(
    mut poll: RestPollDirective,
    graphql: bool,
    graphql_rate_limit: Option<BoundedDelay>,
) -> RestPollDirective {
    if graphql {
        poll.rate_limit = merge_delay(poll.rate_limit, graphql_rate_limit);
    }
    poll
}

fn parse_poll_directive(
    status: u16,
    fields: &[(String, String)],
    structural_error: bool,
) -> (
    RestPollDirective,
    Option<RestReadError>,
    Option<BoundedDelay>,
) {
    let (retry_after, retry_error) = capture_decimal(fields, "retry-after");
    let (remaining, remaining_error) = capture_single(fields, "x-ratelimit-remaining");
    let (reset, reset_error) = capture_decimal(fields, "x-ratelimit-reset");
    let (x_poll_interval, x_poll_error) = capture_decimal(fields, "x-poll-interval");
    let rate_limit = rate_limit_delay(
        status,
        &ParsedRateHeaders {
            retry_after: &retry_after,
            retry_error: retry_error.is_some(),
            remaining,
            remaining_error: remaining_error.is_some(),
            reset: &reset,
            reset_error: reset_error.is_some(),
        },
        structural_error,
    );
    let graphql_rate_limit = graphql_rate_limit_delay(
        status,
        &ParsedRateHeaders {
            retry_after: &retry_after,
            retry_error: retry_error.is_some(),
            remaining,
            remaining_error: remaining_error.is_some(),
            reset: &reset,
            reset_error: reset_error.is_some(),
        },
        structural_error,
    );
    let mut poll = RestPollDirective {
        x_poll_interval: bounded_delay(&x_poll_interval),
        rate_limit,
    };
    if x_poll_error.is_some() {
        poll.x_poll_interval = Some(BoundedDelay::Suspend);
    }
    let error = retry_error
        .or(remaining_error)
        .or(reset_error)
        .or(x_poll_error);
    (poll, error, graphql_rate_limit)
}

fn find_header_end(output: &[u8]) -> Option<(usize, usize)> {
    let crlf = output
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|offset| (offset, 4));
    let lf = output
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .map(|offset| (offset, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        _ => None,
    }
}

fn parse_status(line: &str) -> Result<u16, RestReadError> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut parts = line.split_ascii_whitespace();
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1" | "HTTP/2" | "HTTP/2.0")
        || code.len() != 3
        || !code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        ));
    }
    code.parse().map_err(|_| {
        RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        )
    })
}

fn parse_validators(fields: &[(String, String)]) -> Result<RestValidators, RestReadError> {
    let etag = single(fields, "etag")?
        .map(validate_etag)
        .transpose()?
        .map(str::to_owned);
    let last_modified = single(fields, "last-modified")?
        .map(|value| validate_visible(value, MAX_LAST_MODIFIED_BYTES))
        .transpose()?
        .map(str::to_owned);
    Ok(RestValidators {
        etag,
        last_modified,
    })
}

fn single<'a>(
    fields: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, RestReadError> {
    let mut found = None;
    for (_, value) in fields.iter().filter(|(field, _)| field == name) {
        if found.is_some_and(|prior| prior != value) {
            return Err(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
        }
        found = Some(value.as_str());
    }
    Ok(found)
}

fn single_bounded<'a>(
    fields: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, RestReadError> {
    single(fields, name)?
        .map(|value| validate_visible(value, MAX_DIRECTIVE_BYTES))
        .transpose()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParsedDecimal {
    Missing,
    Malformed,
    Value(u64),
    Overflow,
}

fn capture_decimal(
    fields: &[(String, String)],
    name: &str,
) -> (ParsedDecimal, Option<RestReadError>) {
    match parsed_decimal_header(fields, name) {
        Ok(value) => (value, None),
        Err(error) => (ParsedDecimal::Malformed, Some(error)),
    }
}

fn capture_single<'a>(
    fields: &'a [(String, String)],
    name: &str,
) -> (Option<&'a str>, Option<RestReadError>) {
    match single_bounded(fields, name) {
        Ok(value) => (value, None),
        Err(error) => (None, Some(error)),
    }
}

fn parsed_decimal_header(
    fields: &[(String, String)],
    name: &str,
) -> Result<ParsedDecimal, RestReadError> {
    let Some(value) = single_bounded(fields, name)? else {
        return Ok(ParsedDecimal::Missing);
    };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(ParsedDecimal::Malformed);
    }
    if value.len() > MAX_DECIMAL_DIGITS {
        return Ok(ParsedDecimal::Overflow);
    }
    Ok(value
        .parse()
        .map(ParsedDecimal::Value)
        .unwrap_or(ParsedDecimal::Overflow))
}

fn bounded_delay(value: &ParsedDecimal) -> Option<BoundedDelay> {
    match value {
        ParsedDecimal::Value(seconds) => Some(BoundedDelay::Seconds(*seconds)),
        ParsedDecimal::Overflow => Some(BoundedDelay::Suspend),
        ParsedDecimal::Missing | ParsedDecimal::Malformed => None,
    }
}

struct ParsedRateHeaders<'a> {
    retry_after: &'a ParsedDecimal,
    retry_error: bool,
    remaining: Option<&'a str>,
    remaining_error: bool,
    reset: &'a ParsedDecimal,
    reset_error: bool,
}

fn rate_limit_delay(
    status: u16,
    rate: &ParsedRateHeaders<'_>,
    structural_error: bool,
) -> Option<BoundedDelay> {
    if matches!(status, 403 | 429) {
        if let Some(delay) = bounded_delay(rate.retry_after) {
            return Some(delay);
        }
        if rate.retry_error
            || rate.remaining_error
            || rate.reset_error
            || (status == 429 && structural_error)
        {
            return Some(BoundedDelay::Suspend);
        }
    }
    match status {
        429 => Some(rate_delay(rate.retry_after, rate.remaining, rate.reset)),
        403 if rate.remaining == Some("0") => {
            Some(rate_delay(rate.retry_after, rate.remaining, rate.reset))
        }
        _ => None,
    }
}

fn graphql_rate_limit_delay(
    status: u16,
    rate: &ParsedRateHeaders<'_>,
    _structural_error: bool,
) -> Option<BoundedDelay> {
    if status != 200 {
        return None;
    }
    if let Some(delay) = bounded_delay(rate.retry_after) {
        return Some(delay);
    }
    if rate.retry_error || rate.remaining_error || rate.reset_error {
        return Some(BoundedDelay::Suspend);
    }
    if rate.remaining == Some("0") {
        return Some(rate_delay(rate.retry_after, rate.remaining, rate.reset));
    }
    None
}

fn rate_delay(
    retry_after: &ParsedDecimal,
    remaining: Option<&str>,
    reset: &ParsedDecimal,
) -> BoundedDelay {
    match retry_after {
        ParsedDecimal::Value(seconds) => return BoundedDelay::Seconds(*seconds),
        ParsedDecimal::Overflow => return BoundedDelay::Suspend,
        ParsedDecimal::Missing | ParsedDecimal::Malformed => {}
    }
    if remaining == Some("0") {
        match reset {
            ParsedDecimal::Value(deadline) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|value| value.as_secs());
                if now.is_some_and(|now| *deadline > now) {
                    return BoundedDelay::UntilUnixSeconds(*deadline);
                }
            }
            ParsedDecimal::Overflow => return BoundedDelay::Suspend,
            ParsedDecimal::Missing | ParsedDecimal::Malformed => {}
        }
    }
    BoundedDelay::Seconds(60)
}

fn validate_etag(value: &str) -> Result<&str, RestReadError> {
    if value.len() > MAX_ETAG_BYTES {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidHeaders,
            RestPollDirective::default(),
        ));
    }
    let opaque = value.strip_prefix("W/").unwrap_or(value);
    if opaque.len() < 2
        || !opaque.starts_with('"')
        || !opaque.ends_with('"')
        || opaque[1..opaque.len() - 1]
            .bytes()
            .any(|byte| byte == b'"' || !(0x21..=0x7e).contains(&byte))
    {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidHeaders,
            RestPollDirective::default(),
        ));
    }
    Ok(value)
}

fn validate_visible(value: &str, maximum: usize) -> Result<&str, RestReadError> {
    if value.is_empty()
        || value.len() > maximum
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidHeaders,
            RestPollDirective::default(),
        ));
    }
    Ok(value)
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// Bounded framing for one explicit `gh api --include` mutation response.
///
/// A mutation never reuses the conditional read entrypoint, but its rate and
/// poll directives are still safely parsed so the caller can install the same
/// account floor a read would have installed. Framing failure yields no status
/// and no body, never a guessed success.
pub(super) struct MutationResponseFraming {
    pub(super) status: Option<u16>,
    pub(super) poll: RestPollDirective,
    pub(super) body: Vec<u8>,
}

pub(super) fn parse_mutation_response(output: &[u8]) -> MutationResponseFraming {
    if output.len() > MAX_HEADER_BYTES.saturating_add(CURL_MUTATION_BODY_LIMIT) {
        return MutationResponseFraming {
            status: None,
            poll: RestPollDirective::default(),
            body: Vec::new(),
        };
    }
    let Some((end, separator)) = find_header_end(output) else {
        return MutationResponseFraming {
            status: None,
            poll: rejected_header_poll(&output[..output.len().min(MAX_HEADER_BYTES)], false),
            body: Vec::new(),
        };
    };
    if end > MAX_HEADER_BYTES {
        return MutationResponseFraming {
            status: None,
            poll: RestPollDirective::default(),
            body: Vec::new(),
        };
    }
    let Ok(collected) = collect_header_fields(&output[..end]) else {
        return MutationResponseFraming {
            status: None,
            poll: rejected_header_poll(&output[..end], false),
            body: Vec::new(),
        };
    };
    let (poll, _, _) = parse_poll_directive(collected.status, &collected.fields, false);
    if collected.error.is_some() {
        return MutationResponseFraming {
            status: None,
            poll,
            body: Vec::new(),
        };
    }
    MutationResponseFraming {
        status: Some(collected.status),
        poll,
        body: output[end.saturating_add(separator)..].to_vec(),
    }
}

const CURL_MUTATION_BODY_LIMIT: usize = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_and_bodyless_not_modified_without_inventing_a_body() {
        let response = b"HTTP/2 200 OK\r\nETag: W/\"abc\"\r\nLast-Modified: Sun, 13 Sep 2026 12:00:00 GMT\r\nLink: <https://api.github.com/x?page=2>; rel=\"next\"\r\nX-Poll-Interval: 120\r\n\r\n[]";
        let ConditionalGet::Modified { value, metadata } =
            parse_included_response(response, true).unwrap()
        else {
            panic!()
        };
        assert_eq!(value, b"[]");
        assert_eq!(metadata.validators.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(
            metadata.poll.x_poll_interval,
            Some(BoundedDelay::Seconds(120))
        );
        let ConditionalGet::NotModified { .. } =
            parse_included_response(b"HTTP/2 304 Not Modified\r\nETag: \"abc\"\r\n\r\n", false)
                .unwrap()
        else {
            panic!("304 must stay distinct")
        };
        assert!(parse_included_response(b"HTTP/2 304 Not Modified\r\n\r\n[]", true).is_err());
    }

    #[test]
    fn rate_headers_survive_nonzero_exit_with_precedence_and_no_remote_text() {
        let error = parse_included_response(b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 9999999999\r\n\r\nprivate remote body", false).unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));
        assert!(!error.to_string().contains("private"));

        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        let response = format!(
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n"
        );
        let error = parse_included_response(response.as_bytes(), false).unwrap_err();
        assert_eq!(
            error.poll().rate_limit,
            Some(BoundedDelay::UntilUnixSeconds(reset))
        );
        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: invalid\r\n\r\n",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(60)));
        let error = parse_included_response(b"HTTP/2 403 Forbidden\r\n\r\n", false).unwrap_err();
        assert_eq!(error.poll().rate_limit, None);
    }

    #[test]
    fn rejects_conflicting_duplicates_crlf_injection_and_oversize_headers() {
        assert!(
            parse_included_response(
                b"HTTP/2 200 OK\r\nETag: \"a\"\r\nETag: \"b\"\r\n\r\n[]",
                true
            )
            .is_err()
        );
        assert!(
            parse_included_response(
                b"HTTP/2 200 OK\r\nX-Poll-Interval: 1\rX-Bad: yes\r\n\r\n[]",
                true
            )
            .is_err()
        );
        let oversized = format!(
            "HTTP/2 200 OK\r\nX-Fill: {}\r\n\r\n[]",
            "x".repeat(MAX_HEADER_BYTES)
        );
        assert!(parse_included_response(oversized.as_bytes(), true).is_err());
    }

    #[test]
    fn numeric_delay_overflow_suspends_and_survives_unrelated_bad_metadata() {
        for value in ["18446744073709551616", "999999999999999999999"] {
            let response = format!("HTTP/2 200 OK\r\nX-Poll-Interval: {value}\r\n\r\n[]");
            let ConditionalGet::Modified { metadata, .. } =
                parse_included_response(response.as_bytes(), true).unwrap()
            else {
                panic!()
            };
            assert_eq!(metadata.poll.x_poll_interval, Some(BoundedDelay::Suspend));
        }
        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nETag: contains controls\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));

        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Remaining: 1\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));
        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nX-Poll-Interval: 10\r\nX-Poll-Interval: 20\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));
        assert_eq!(error.poll().x_poll_interval, Some(BoundedDelay::Suspend));
        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nRetry-After: 91\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));

        let error = parse_included_response(
            b"HTTP/2 403 Forbidden\r\nRetry-After: 90\r\nRetry-After: 91\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));
        for status in [403, 429] {
            let response = format!(
                "HTTP/2 {status} limited\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 9999999998\r\nX-RateLimit-Reset: 9999999999\r\n\r\nprivate"
            );
            let error = parse_included_response(response.as_bytes(), false).unwrap_err();
            assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));
        }
        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nX-RateLimit-Reset: 9999999998\r\nX-RateLimit-Reset: 9999999999\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));
        let error = parse_included_response(
            b"HTTP/2 200 OK\r\nX-Poll-Interval: 90\r\nX-RateLimit-Reset: 9999999998\r\nX-RateLimit-Reset: 9999999999\r\n\r\n[]",
            true,
        )
        .unwrap_err();
        assert_eq!(
            error.poll().x_poll_interval,
            Some(BoundedDelay::Seconds(90))
        );
        assert_eq!(error.poll().rate_limit, None);
    }

    #[test]
    fn scheduling_recovery_never_reads_body_lookalikes() {
        let poll = general_poll_from_included_prefix(
            b"HTTP/2 200 OK\r\nETag: \"safe\"\r\n\r\nbody\nx-poll-interval: 999999999999999999999",
        );
        assert_eq!(poll, RestPollDirective::default());
    }

    #[test]
    fn graphql_ambiguous_exhaustion_metadata_suspends_without_shortening_the_floor() {
        let error = parse_graphql_included_response(
            b"HTTP/2 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 9999999998\r\nX-RateLimit-Reset: 9999999999\r\n\r\n{\"errors\":[{\"message\":\"rate limited\"}]}",
            true,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));

        let error = parse_graphql_included_response(
            b"HTTP/2 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Remaining: 1\r\nX-RateLimit-Reset: 9999999999\r\n\r\n{\"errors\":[{\"message\":\"rate limited\"}]}",
            true,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));
    }

    #[test]
    fn post_status_structural_errors_keep_safe_independent_floors() {
        for status in ["403 Forbidden", "429 Too Many Requests"] {
            let response = format!("HTTP/2 {status}");
            let error = parse_included_response(response.as_bytes(), false).unwrap_err();
            assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));
        }

        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\n continuation\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));

        let error = parse_included_response(
            b"HTTP/2 429 Too Many Requests\r\nX-Unrelated: ok\r\nbroken\r\n\r\nprivate",
            false,
        )
        .unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Suspend));

        let mut too_many = String::from("HTTP/2 200 OK\r\nX-Poll-Interval: 90\r\n");
        for index in 0..MAX_HEADER_FIELDS {
            too_many.push_str(&format!("X-Fill-{index}: ok\r\n"));
        }
        too_many.push_str("\r\n[]");
        let error = parse_included_response(too_many.as_bytes(), true).unwrap_err();
        assert_eq!(
            error.poll().x_poll_interval,
            Some(BoundedDelay::Seconds(90))
        );

        let oversized = format!(
            "HTTP/2 429 Too Many Requests\r\nRetry-After: 90\r\nX-Fill: {}\r\n\r\nprivate",
            "x".repeat(MAX_HEADER_BYTES)
        );
        let error = parse_included_response(oversized.as_bytes(), false).unwrap_err();
        assert_eq!(error.poll().rate_limit, Some(BoundedDelay::Seconds(90)));

        for (status, prior, name, expected_poll, expected_rate) in [
            (
                "429 Too Many Requests",
                "",
                "Retry-After",
                None,
                Some(BoundedDelay::Suspend),
            ),
            (
                "200 OK",
                "",
                "X-Poll-Interval",
                Some(BoundedDelay::Suspend),
                None,
            ),
            (
                "429 Too Many Requests",
                "Retry-After: 90\r\n",
                "Retry-After",
                None,
                Some(BoundedDelay::Suspend),
            ),
            (
                "200 OK",
                "X-Poll-Interval: 90\r\n",
                "X-Poll-Interval",
                Some(BoundedDelay::Suspend),
                None,
            ),
        ] {
            let mut response = format!("HTTP/2 {status}\r\n{prior}").into_bytes();
            let partial = format!("{name}: 90");
            let fill = MAX_HEADER_BYTES
                .checked_sub(response.len() + "X-Fill: \r\n".len() + partial.len())
                .unwrap();
            response.extend_from_slice(b"X-Fill: ");
            response.extend(std::iter::repeat_n(b'x', fill));
            response.extend_from_slice(b"\r\n");
            response.extend_from_slice(partial.as_bytes());
            assert_eq!(response.len(), MAX_HEADER_BYTES);
            response.extend_from_slice(b"000000000000000000\r\n\r\nprivate");
            let error = parse_included_response(&response, false).unwrap_err();
            assert_eq!(error.poll().x_poll_interval, expected_poll);
            assert_eq!(error.poll().rate_limit, expected_rate);
        }
    }
}
