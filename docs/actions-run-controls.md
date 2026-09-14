# GitHub Actions run controls

The Checks inspector can re-run all jobs, re-run only failed jobs, or cancel one exactly identified GitHub Actions workflow run. These are the only three controls. There is no force-cancel, no arbitrary workflow dispatch, no run deletion, and no CI settings change.

## User flow

1. Open **Checks** and select a fully identified linked Actions check run. The controls appear beside **Load exact Actions jobs** and use the same exact identity.
2. Choose **Re-run all jobs**, **Re-run failed jobs**, or **Cancel run**. Nothing is sent yet: the application reads the selected viewer, the repository's current permissions, and the exact current run.
3. A confirmation states the repository, pull request, selected account and viewer node ID, workflow, run ID, run number, attempt, event, current status and conclusion, head commit, selected check, the fresh permission evidence, and the exact method, path, and body that confirmation will send.
4. Confirm sends one request. Cancel discards the prepared request and sends nothing.

Preparation reads nothing through the automatic read scheduler and admits no read operation. A control shares this pull request's one durable per-target mutation authority with review, auxiliary, merge, lifecycle, discussion, reaction, and dismissal writes, so an unresolved attempt in any of those families blocks the others until it is explicitly reconciled.

## Exact identity and attempt

Admission requires the same complete Checks identity the Jobs reader requires. Preparation then reads `repos/{owner}/{repo}/actions/runs/{run_id}` and requires the run's node ID, database ID, run number, event, workflow ID, check-suite node and database IDs, head commit, API URL, HTML URL, workflow URL, repository identity, and head-repository identity all to match the selected check.

The run's current attempt must equal the attempt the inspector displayed. A run that has advanced is refused by name, stating both attempts. A historical display never silently retargets a later attempt.

GitHub's re-run endpoints act on the run, not on a chosen attempt, and a successful re-run produces a new attempt that this feature does not follow.

## Permission

Write permission comes only from a fresh `repos/{owner}/{repo}` read. Resolving `/user` proves which account is selected; it never implies what that account may write.

- A returned permissions object with `push`, `maintain`, or `admin` is **Available**.
- A missing permissions object is **Unknown**: the confirmation states that GitHub decides authorization when the request is sent.
- A permissions object without write access, or an archived repository, is **Unavailable** and the control is refused before any durable admission.

A frozen request records the evidence it was prepared with. That record is never reused as capability: the post-admission preflight re-reads the viewer, the permissions, and the run, and compares the whole observation.

## Action and status

- **Cancel run** requires a run that is `queued`, `in_progress`, `waiting`, `requested`, or `pending`.
- **Re-run all jobs** and **Re-run failed jobs** require a `completed` run.
- **Re-run failed jobs** additionally refuses a run whose conclusion is absent, `success`, or `skipped`, because such a run identifies no failed job.

These refusals keep an obvious rejection from crossing the transport and leaving an unresolved record behind. GitHub remains the authority; a combination that passes this check can still be refused.

## Dispatch and outcome

The exact method, path, body, and action name are frozen once. The same `MutationContext` backs the durable journal admission, the POST, and every post-send local failure record. The body is always an empty JSON object; `enable_debug_logging` is never sent.

Dispatch runs `gh api --include`, so the response's status line and headers are parsed directly.

| Control | Path | Accepted status |
| --- | --- | ---: |
| Re-run all jobs | `POST repos/{owner}/{repo}/actions/runs/{run_id}/rerun` | 201 |
| Re-run failed jobs | `POST repos/{owner}/{repo}/actions/runs/{run_id}/rerun-failed-jobs` | 201 |
| Cancel run | `POST repos/{owner}/{repo}/actions/runs/{run_id}/cancel` | 202 |

**An accepted status means GitHub accepted the request.** It is not proof that a new attempt started, that any job re-ran, or that the run is cancelled. After acceptance the application performs one read-only observation of the run and reports it as observed state, explicitly not as an effect attributed to the request.

Outcomes are classified in exactly three ways:

- **Zero writes sent.** Local validation failed, durable admission failed or returned a mismatched receipt, the post-admission preflight refused, the frozen body could not be encoded, the credential could not be resolved, or the transport process never started. A durable NotStarted record is written.
- **Accepted.** The documented status arrived with an empty or empty-object body.
- **Unresolved.** Everything else, without exception: any other framed status including 403, 404, 409, 422 and 5xx; the documented status with an unexpected body; an unframable response; an oversized response; and a lost acknowledgement. A request that reached GitHub is never recorded as a proven no-op on the strength of its status alone, because nothing in the response separates a refusal from a refusal after an effect.

An unresolved attempt is frozen against replay. There is no automatic retry anywhere in this feature.

If a terminal record cannot be saved, the outcome stays Uncertain and durable InFlight authority is retained — including for an attempt that provably sent zero writes.

## Rate and poll directives

Rate-limit and poll headers are safely parsed from every dispatch and installed into the same per-account floor a read would have installed. That evidence lives in the bounded leading header block, so it is recovered independently of the body and of classification: an error status, an unframable response, a body over its bound, and a response too large to classify all still install whatever floor the server stated. A transport that starts and then fails — timing out, overflowing its output bound, or dying — is no exception: the dispatch entrypoint retains the same bounded leading stdout header prefix the conditional read path already captures, and the floor is installed from that. Only stdout may be scheduling evidence; child stderr is never read, never parsed, never retained, and cannot install a floor however closely it resembles a header block. A retained prefix is cut at the first header delimiter, so it never carries body bytes that happened to arrive in the same read, and it is returned only to this dispatch: it never records into the shared read collector, even when one is active. A transport that never started has no prefix and installs nothing. Recovering a prefix never changes the outcome: a started transport that failed stays unresolved. Installing that floor admits and releases no read operation: a mutation never uses the automatic read scheduler as its authority, and a mutation never becomes a read. There is no retry-after sleep, no backoff loop, and no automatic resend.

## Reconciliation

**Reconcile auxiliary / merge actions** in Activity performs one read-only observation for an unresolved control.

- A re-run resolves only when the run shows an attempt later than the frozen one.
- A cancel resolves only when the frozen attempt itself is now `completed` with conclusion `cancelled`.

Both outcomes are recorded as observed movement, not as an effect caused by this attempt: GitHub records no per-request Actions control event. An unmoved run never proves the attempt was NotApplied, so it stays unresolved.

## Preserved behavior

Controls never touch the displayed comparison, the pinned canonical revision, any local review draft, any pinned selection, or the memory-only Jobs and Log panes. A live pending-review start blocks a control, and another in-flight mutation in this tab blocks it.

## Limits

- GitHub exposes no expected-attempt, expected-status, or expected-state condition on any of the three endpoints. No control is atomic, and the run can move between the preflight and the request.
- Acceptance is never completion. This feature does not poll for, wait on, or report the result of a re-run or a cancellation.
- A re-run creates a new attempt that this feature does not load, select, or follow.
- Rate limiting installs an account floor; it never schedules a resend.
- No live-provider fixture is part of automated verification. Every test uses a synthetic `gh` transport and disposable local state, and no test performs a real remote mutation.
- The controls and both confirmation buttons track caller-owned focus handles and are verified to accept focus, but keyboard operation is not verified. In this project's GPUI test context neither a Tab keystroke, nor programmatic focus traversal, nor Enter/Space on a focused button reaches a `gpui_base::Button`. That is app-wide rather than specific to these controls: the pre-existing Checks revision-details button behaves the same way, because this inspector declares no tab group. Native verification therefore covers pointer activation only, and no keyboard-operability claim is made for this feature.
- This is not complete GitHub Actions parity. Workflow dispatch, job-level re-runs, run deletion, approval of deployments and of fork workflow runs, artifact and cache management, and Actions settings remain absent.
