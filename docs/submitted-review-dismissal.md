# Submitted-review dismissal

This slice implements bounded native dismissal for an exact GitHub pull-request review. It applies only to reviews whose fresh state is `APPROVED` or `CHANGES_REQUESTED`. It does not require the review to belong to the selected viewer, to refer to the current head commit, or to belong to an open pull request.

## Fresh capability and preparation

Activity details attach `FreshReviewDismissalCapability` only to the in-memory response. The field uses `#[serde(skip)]`, so neither serialization nor deserialization can persist or manufacture it. Cached or partial activity is always read-only.

Direct fresh `Repository.viewerCanAdminister` evidence produces `Available`. The absence of that evidence does not infer denial from review authorship, repository role, branch state, team membership, or incomplete data. An otherwise complete eligible read produces `Unknown`, and the UI explicitly says that GitHub will decide authorization. `Unavailable` and ineligible review states never arm preparation.

Preparation performs a targeted GraphQL read and freezes:

- the selected account's viewer node ID and login;
- the selected repository/account and exact pull-request and review coordinates;
- state, body, submission time, nullable author, and nullable exact commit;
- the fresh authority result;
- a required, nonblank reason bounded to 64 KiB;
- distinct operation and attempt IDs.

GitHub's schema allows the review author and commit to be explicit `null`. Those values remain `None`; they are never synthesized. Omission of either requested field is malformed/partial and rejects preparation. The same omitted-versus-null rule applies to acknowledgement decoding.

The reason uses a separate native textarea. Before confirmation it is retained only in the owning in-memory tab and review lane. Cancellation retains it. Target switches retain each review's separate draft. It is not durably persisted before confirmation, so closing the tab, terminating the process, or a crash can lose it. LINE/FILE composer text and submitted-review text continue through their existing persistence and close barriers; dismissal transitions do not clear them.

## Confirmation and dispatch

Confirmation shows the exact frozen account, repository, PR, review, previous state/body/submission time, nullable author/commit, reason, and authority disclosure. Editing the reason or switching targets invalidates the confirmation. The confirm handler again admits the current visible input owner and requires its active target and reason to equal the frozen request; otherwise it retains the new text and dispatches zero writes.

After explicit confirmation, one distinct `JournalRequest::Dismissal` is recorded through the shared per-PR durable mutation authority. Only after durable admission does the provider perform a second exact targeted preflight. Any identity, viewer, authority, state, body, submission time, author, or commit change records `NotStarted` and sends zero writes.

The sole write is GitHub's `dismissPullRequestReview` GraphQL mutation with exactly `pullRequestReviewId`, the frozen `message`, and the frozen operation ID as `clientMutationId`. There is no retry, fallback, or second mutation. GitHub does not expose an expected-state/body/commit compare-and-swap field for this mutation, so another actor can still change the review in the short interval after the second preflight. The confirmation discloses that race.

An acknowledgement is accepted only when the response echoes the operation ID, identifies a `PullRequestReview`, returns `DISMISSED`, preserves the complete frozen tuple including explicit nullable author/commit, and acknowledges the same parent PR/repository. A missing, partial, malformed, or mismatched acknowledgement becomes `Uncertain`. Failure to save an acknowledged terminal record also becomes `Uncertain` while retaining durable `InFlight` authority. Neither case is replayed automatically.

## Recovery boundary

Read-only reconciliation may query the known review ID and report its exact current state, including `DISMISSED`. Current state alone cannot prove which attempt caused dismissal or which message GitHub recorded. This slice therefore keeps dismissal uncertainty unresolved unless a future separately exact event observation can match the frozen review, parent, selected actor, previous state, and message without inferring causation.

The durable journal remains compatible with legacy version-1 records. A pending or uncertain dismissal shares the existing per-PR barrier with other review mutations and blocks replay after restart.

## Official API references

- [GitHub GraphQL pull-request mutations and objects](https://docs.github.com/en/graphql/reference/pulls)
- [GitHub GraphQL repository fields](https://docs.github.com/en/graphql/reference/repos)
- [GitHub REST pull-request reviews](https://docs.github.com/en/rest/pulls/reviews)
