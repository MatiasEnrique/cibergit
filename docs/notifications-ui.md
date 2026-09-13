# Native unread notifications

cibergit keeps notification unread state locally and partitioned by the exact selected GitHub account. The sidebar shows small per-pull-request unread counts and the Alerts panel shows a bounded page of exact supported events. Review requests, native PR-body mentions, review-thread replies, and failed checks on the selected account's own pull requests remain distinct. Candidates lacking authoritative event evidence appear in a separate incomplete section and never increase unread counts.

Polling is read-only. Configured repositories are grouped by exact account and read in sequential batches of at most five repositories. A slow account has at most one observation/reconcile lane. Partial repositories and pending/failed batches remain visible; an offline failure retains the prior cached unread snapshot instead of presenting a false empty result. Error backoff and inactive-window slowdown use the existing polling schedule, independently for each account. Partial provider results retain backoff; stale selection/controller callbacks cannot reset it. Removed repositories are filtered from both poll and mark-read snapshots without erasing their durable unread events. Returning to the same selection does not revive an older callback.

Opening an event routes only to the exact account, repository, and pull request already configured in the workspace. It does not mark remote GitHub notifications read and does not advance a previously selected review comparison. Closing the panel does not clear anything. “Mark displayed read” captures the rendered state version and exact event IDs, so an event admitted concurrently remains unread.

GitHub documents the native `mentioned` event actor as the recipient in an issue or PR body. Classification requires that recipient to match the account and validates the immutable event ID, repository URL and timestamp. The recipient is not attributed as the author. Sticky notification reasons, comment text and team mentions remain insufficient evidence. See [GitHub issue event types](https://docs.github.com/en/rest/using-the-rest-api/issue-event-types#mentioned).

## macOS opt-in

macOS alerts are an explicit app-local preference for each account and default to off. Enabling is durably saved before a future newly admitted event may be offered to the operating system. Enabling never sends historical catch-up alerts. Disabling invalidates pending UI dispatch. Preference, stable event-tag deduplication, and exact callback routes are stored in a private bounded registry using a no-follow lock and atomic compare-and-swap replacement.

The app uses pinned GPUI's `show_system_notification` API. On macOS, GPUI uses `UNUserNotificationCenter`, asks for authorization lazily on first show, and no-ops when the process is outside an app bundle or delivery is unavailable. GPUI provides no permission or delivery receipt, so cibergit reports only that an eligible alert was requested best effort. It never claims that permission was granted or a notification was delivered.

System-notification responses resolve only retained local stable tags. Unknown or expired tags do nothing. A known response may open its exact configured pull request; it never creates an account, changes remote unread state, or silently advances the reviewed revision.

## Verification boundary

Unit fixtures exercise the real local store baseline/new-event/restart/dedup path, account and callback isolation, exact displayed marking with a concurrent event, five-plus repository batching, durable default-off consent, stable tags, callback routing, and late/stale-result rejection. UI smoke uses a synthetic exact event through the real controller completion handler and a test sink. The `ui-smoke` build hard-suppresses GPUI notification calls, so it cannot prompt for permission or post a real system notification.

Light/dark captures establish the native GPUI presentation and controller state only. A final packaged `.app` consent test, physical macOS delivery, comment and team mention coverage, conditional HTTP caching, and the broader M2/V1 acceptance remain external/open.
