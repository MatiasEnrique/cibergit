//! Bounded parser and metadata for `gh api --include` notification reads.
//! Diagnostics deliberately contain no remote body, stderr, token, or header values.

use serde::{Deserialize, Serialize};
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_FIELDS: usize = 128;
const MAX_ETAG_BYTES: usize = 1024;
const MAX_LAST_MODIFIED_BYTES: usize = 128;
const MAX_LINK_BYTES: usize = 16 * 1024;
const MAX_DIRECTIVE_BYTES: usize = 32;
const MAX_DECIMAL_DIGITS: usize = 20;

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
    Transport,
    OperationLimit,
    InvalidFraming,
    InvalidHeaders,
    InvalidBody,
    HttpFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestReadError {
    kind: RestReadErrorKind,
    poll: RestPollDirective,
}

impl RestReadError {
    fn new(kind: RestReadErrorKind, poll: RestPollDirective) -> Self {
        Self { kind, poll }
    }
    pub(crate) fn credential() -> Self {
        Self::new(RestReadErrorKind::Credential, RestPollDirective::default())
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
    pub(crate) fn invalidates_cached_body(&self) -> bool {
        matches!(self.kind, RestReadErrorKind::InvalidBody)
    }
    fn with_poll(mut self, poll: RestPollDirective) -> Self {
        self.poll = poll;
        self
    }
}

impl fmt::Display for RestReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            RestReadErrorKind::Credential => "Selected GitHub credential is unavailable",
            RestReadErrorKind::Transport => "GitHub conditional read transport failed",
            RestReadErrorKind::OperationLimit => "GitHub conditional read operation limit reached",
            RestReadErrorKind::InvalidFraming => "Invalid GitHub included-response framing",
            RestReadErrorKind::InvalidHeaders => "Invalid GitHub response metadata",
            RestReadErrorKind::InvalidBody => "Invalid or incomplete GitHub JSON response",
            RestReadErrorKind::HttpFailure => "GitHub conditional read was rejected",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RestReadError {}

pub(crate) fn parse_included_response(
    output: &[u8],
    process_success: bool,
) -> Result<ConditionalGet<Vec<u8>>, RestReadError> {
    let (header_end, delimiter_len) = find_header_end(output).ok_or_else(|| {
        RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        )
    })?;
    if header_end > MAX_HEADER_BYTES {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidHeaders,
            RestPollDirective::default(),
        ));
    }
    let header = &output[..header_end];
    if header.contains(&0)
        || header
            .iter()
            .enumerate()
            .any(|(index, byte)| *byte == b'\r' && header.get(index + 1) != Some(&b'\n'))
    {
        return Err(RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        ));
    }
    let header_text = std::str::from_utf8(header).map_err(|_| {
        RestReadError::new(
            RestReadErrorKind::InvalidHeaders,
            RestPollDirective::default(),
        )
    })?;
    let mut lines = header_text.lines();
    let status_line = lines.next().ok_or_else(|| {
        RestReadError::new(
            RestReadErrorKind::InvalidFraming,
            RestPollDirective::default(),
        )
    })?;
    let status = parse_status(status_line)?;
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in lines {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if fields.len() == MAX_HEADER_FIELDS
            || line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with("HTTP/")
        {
            return Err(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            )
        })?;
        if name.is_empty()
            || !name.bytes().all(is_token)
            || value
                .bytes()
                .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n')
        {
            return Err(RestReadError::new(
                RestReadErrorKind::InvalidHeaders,
                RestPollDirective::default(),
            ));
        }
        fields.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let (retry_after, retry_error) = capture_decimal(&fields, "retry-after");
    let (remaining, remaining_error) = capture_single(&fields, "x-ratelimit-remaining");
    let (reset, reset_error) = capture_decimal(&fields, "x-ratelimit-reset");
    let rate_limit = match status {
        429 if retry_error.is_some() => Some(BoundedDelay::Suspend),
        429 => Some(rate_delay(&retry_after, remaining, &reset)),
        403 if retry_after.is_present() || remaining == Some("0") => {
            Some(rate_delay(&retry_after, remaining, &reset))
        }
        _ => None,
    };
    let mut poll = RestPollDirective {
        x_poll_interval: None,
        rate_limit,
    };
    if let Some(error) = retry_error.or(remaining_error).or(reset_error) {
        return Err(error.with_poll(poll));
    }
    poll.x_poll_interval = match delay_header(&fields, "x-poll-interval") {
        Ok(delay) => delay,
        Err(error) => {
            poll.x_poll_interval = Some(BoundedDelay::Suspend);
            return Err(error.with_poll(poll));
        }
    };
    let validators = parse_validators(&fields).map_err(|error| error.with_poll(poll.clone()))?;
    let link = single(&fields, "link")
        .and_then(|value| {
            value
                .map(|value| validate_visible(value, MAX_LINK_BYTES))
                .transpose()
        })
        .map_err(|error| error.with_poll(poll.clone()))?
        .map(str::to_owned);
    let body = &output[header_end + delimiter_len..];
    let metadata = RestResponseMetadata {
        validators,
        link,
        poll: poll.clone(),
        body_bytes: body.len(),
    };
    match status {
        200 if process_success && !body.is_empty() => Ok(ConditionalGet::Modified {
            value: body.to_vec(),
            metadata,
        }),
        304 if body.is_empty() => Ok(ConditionalGet::NotModified { metadata }),
        200 | 304 => Err(RestReadError::new(RestReadErrorKind::InvalidFraming, poll)),
        _ => Err(RestReadError::new(RestReadErrorKind::HttpFailure, poll)),
    }
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

impl ParsedDecimal {
    fn is_present(self) -> bool {
        !matches!(self, Self::Missing | Self::Malformed)
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

fn delay_header(
    fields: &[(String, String)],
    name: &str,
) -> Result<Option<BoundedDelay>, RestReadError> {
    Ok(match parsed_decimal_header(fields, name)? {
        ParsedDecimal::Value(seconds) => Some(BoundedDelay::Seconds(seconds)),
        ParsedDecimal::Overflow => Some(BoundedDelay::Suspend),
        ParsedDecimal::Missing | ParsedDecimal::Malformed => None,
    })
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
            .any(|byte| byte == b'"' || byte < 0x21 || byte > 0x7e)
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
    }
}
