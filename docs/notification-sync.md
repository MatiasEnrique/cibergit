# Conditional notification sync

Cibergit conditionally reads only the selected-repository notification pages. Pull request, timeline, review-comment, check-run, GraphQL, and mutation requests keep their existing behavior.

The first read of a page is an ordinary `GET`. When GitHub returns a valid `ETag`, the next request sends `If-None-Match`. Without an ETag, a valid `Last-Modified` value produces `If-Modified-Since`. A fresh `200` replaces the validator set, including removing validators that the new response omitted.

A `304 Not Modified` has no discovery meaning by itself. The provider accepts it only with the exact cached page for the same account, host, repository, method, path, canonical query, page, optional `since`, media type, and API version. It reuses that page's items and validated next-page relation. A zero-cache `304` makes the repository incomplete. It never becomes an empty or terminal page.

## Cache limits

The discovery cache lives in memory and contains provider notification pages, not notification events. It has these limits:

- 100 items per page
- 16 KiB per item's canonical JSON representation
- 2 MiB per cache entry's canonical JSON representation
- 50 entries
- 8 MiB across all retained canonical JSON entry representations

The entry representation used for byte accounting includes the full cache key, validators, parsed next-page number, and retained items. These are serialized-payload limits. They are not a claim about allocator overhead or total process heap use.

An oversized response can still complete a fresh `200` read if it passes the existing notification bounds and validation. The provider simply does not retain it for a later conditional request. Selection or account removal clears the affected cache. A selection change starts that account's cache over. Terminal pages remove old tail entries, and count or byte pressure removes entries in canonical key order.

Every cached candidate goes through current hydration again. The provider reads the pull request, timeline, review comments, and check runs, then classifies immutable evidence. Cached discovery cannot authorize an alert or a write. If one candidate fails hydration, it contributes no invented evidence and an incomplete repository cannot create a baseline. An independently hydrated event in the same mixed batch can still alert when the durable baseline already exists.

## Pagination

The provider validates `rel="next"` against `https://api.github.com`, the selected repository path, the same query, and the immediately following page. A foreign host, changed query, malformed relation, or skipped page makes the repository incomplete. Without `Link`, a page with fewer than 100 items is terminal. A 100-item page still requests the next numbered page.

Cached pages count against the same item and page limits as fresh pages. Duplicate thread IDs across any mix of `200` and `304` pages fail the read as a moving pagination result.

## Server polling gate

Notification polling tracks an in-memory gate per account. The controller checks it for timer, focus, manual, action-triggered, and startup refresh requests. No trigger bypasses the gate.

`X-Poll-Interval` supplies a minimum delay after an accepted response. For `403` and `429` rate responses, `Retry-After` takes precedence. Otherwise a zero `X-RateLimit-Remaining` value can use `X-RateLimit-Reset`. A `429` without a usable directive waits at least 60 seconds. The existing local failure backoff continues independently, and a later success cannot erase an unexpired server deadline.

The controller converts relative delays with monotonic time and checked arithmetic. An unrepresentable delay suspends polling instead of shortening the server's minimum. A stale selection completion cannot install its cache, snapshot, or alerts. It may still apply a rate-limit delay when the controller lifetime and exact account match, which protects the same credential after a selection change.

The response parser caps the header prefix at 64 KiB and 128 fields. It also caps ETag, Last-Modified, Link, and scheduling values. Invalid framing, controls, conflicting duplicates, oversized values, unexpected statuses, and a nonempty `304` fail closed. Errors expose only fixed categories and safe polling directives. They do not include response bodies, subprocess stderr, tokens, or arbitrary remote text.
