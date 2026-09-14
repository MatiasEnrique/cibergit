# GitHub Actions jobs and logs

The Checks inspector can load the jobs for one exactly identified GitHub Actions run attempt and then load one selected job log. These reads are deliberately separate from the displayed comparison and from every review or CI mutation path.

## User flow

1. Open **Checks** and select a fully identified linked Actions check run.
2. Choose **Load exact Actions jobs**. The Jobs pane labels the repository, run ID, attempt, commit, selected viewer, observation, and whether GitHub returned a current, historical, or unknown pull-request association.
3. Select a job with the pointer or Up/Down. Jobs are paged 40 at a time; PageUp/PageDown moves pages. Job REST IDs remain distinct from check-run database IDs.
4. Choose **Load log** or press Enter. Escape returns from Log to Jobs and from Jobs to Checks.

Jobs and logs live only in the current tab's memory. A refresh preserves a selected job only when the exact attempt still matches. Changed checks, attempts, details generations, job selections, tabs, repositories, pull requests, accounts, or workspace lifetimes cancel or fence prior work. Detached content is labelled historical and cannot admit a new log read.

The Log pane renders bounded, sanitized plain text. It virtualizes at most 128 rows per render request, supports vertical navigation and horizontal reach for long lines, and never interprets ANSI/terminal controls, Markdown, HTML, or URLs.

## Identity and bounds

Admission requires complete Checks identity evidence: selected account and repository; PR node/number/head; rollup repository/SHA; check node/database ID/repository/SHA; suite node/database ID/repository; workflow node/database ID; and the exact workflow run node/database ID, run number, attempt, event, and canonical `https://github.com/{owner}/{repo}/actions/runs/{run_id}` URL. Display names, app slugs, details URLs, permalinks, and decoded opaque IDs cannot repair missing identity.

Jobs resolve `/user` only after the shared selected-account read controller admits the operation. The provider then reads the exact attempt, enumerates attempt-specific pages, revalidates every page, and revalidates the run before returning a complete snapshot. Limits are 100 jobs per page, 10 pages, 1,000 jobs, 256 steps per job, 10,000 final steps, 8 MiB cumulative included JSON, 64 KiB headers, 30 seconds per child, and 180 seconds for the whole operation. An empty or omitting REST pull-request association remains **Unknown**.

Logs revalidate the exact selected job and viewer under one 90-second monotonic deadline. Raw body size is at most 8 MiB, normalized logical rows at most 200,000, and a normalized row at most 256 KiB. CRLF and lone CR become LF; invalid UTF-8 and non-tab C0/C1 controls become visible replacement characters. Partial, malformed, oversized, timed-out, cancelled, or identity-moving responses never install a fresh buffer.

## Transport and privacy

All requests are GET-only. Jobs use locally generated GitHub API routes. Log download uses `/usr/bin/curl` directly, with `-q` first, no shell, no netrc, no proxy, no automatic redirects, and a cleared child environment.

The authenticated API process receives its URL and Authorization header only through bounded, escaped stdin curl configuration. GitHub's signed storage URL is accepted only for lower-case HTTPS subdomains of `actions.githubusercontent.com` or `blob.core.windows.net`, with no userinfo, fragment, IP literal, port, controls, or whitespace. Each of at most two storage redirects starts a fresh credential-free process. Storage hops receive no Authorization, GitHub token environment, cookies, referer, API media type/version header, proxy/debug/TLS key-log controls, or inherited curl configuration.

Tokens, signed URL/path/query values, child stderr, response bodies, and raw headers are withheld from diagnostics. Signed locations and credentials are never persisted. Consumed secret buffers are cleared on a best-effort basis; this is not a claim of full heap erasure.

There is no details-URL fetch, run ZIP fallback, archive extraction, implicit link action, live terminal rendering, CI mutation, or CI settings change in this feature.

## Failure labels

The UI reports closed categories without remote text: credential, permission, rate limited, unavailable, log not ready, unsupported storage host, moved identity, too large, timed out, cancelled, invalid response, or transport failure. Safely parsed API scheduling floors survive nonzero exits, overflow, malformed framing, cancellation, and timeout; stderr and storage bodies never steer the account scheduler.

No live-provider fixture is part of automated verification. A later explicitly authorized opt-in fixture must use the coordinator's one exact public `cli/cli` linked run/job, with no fallback substitution after failure. It remains GET-only and retains only sanitized methods, counts, statuses, categories, exact nonsensitive identities, media type, storage hostname, redirect count, and source/binary hashes—never a token, signed location/path/query, raw header, or log body.

## Native verification harness

The `ui-smoke` test harness can install an internal synthetic Jobs/Log result sink. The sink is consulted only after the real Root entrypoint and selected-account controller admit a read; completion still returns through the normal asynchronous controller release, strict lifetime token, and tab apply path. Dispatch counters distinguish those synthetic sink calls from provider calls. The sink has no credential, `/user`, API, or storage capability and is not live-provider evidence.

The focused native test requires the 200,000th row to be materialized by the virtual list, verifies at most 128 rows per render request, and requires meaningful horizontal overflow before and after moving to the far-right sentinel. GPUI's test context does not provide this project's native renderer, so that test is geometry and materialization evidence, not screenshot evidence. Separately labelled light/dark PNG capture belongs to the native executable smoke gate.

The native executable scene is opt-in with `CIBERGIT_SMOKE_ACTIONS_JOBS_LOGS=1`, `CIBERGIT_SMOKE_DIR`, `CIBERGIT_SMOKE_APPEARANCE=light|dark`, and `CIBERGIT_SMOKE_BACKGROUND=1`. It must run with a new disposable `CIBERGIT_DATA_DIR` and no repository, account, or pull-request arguments. That exact smoke mode sets the test-only bootstrap read kill switch before the workspace is created and refuses scene installation unless accounts, repositories, and tabs are initially empty. It emits one Jobs image, left- and far-right final-row images, and a count-only report for each appearance; every filename and visible fixture label includes `synthetic`.

The executable scene uses the same post-admission zero-capability sink as the focused test. Render-side counters require at least one materialization of row 200,000 and reject any virtual-list request above 128 rows. The scene also requires the enclosing inspector to reach its measured bottom so the Log viewport is on-screen. The right image is attempted only after the horizontal maximum exceeds 1,000 pixels, the left image succeeded with the list at its final row, and the horizontal scroll handle was moved to that measured maximum. Its report contains only synthetic dispatch/render counts, geometry, filenames, and capability declarations—never log contents or provider response data.

Any eventual real read-only preparation remains a distinct evidence phase governed by the exact `cli/cli` fixture restriction above. A synthetic admission/completion or scene pass must never be reported as live API or storage compatibility, and a geometry-only test must not be reported as visible screenshot proof.
