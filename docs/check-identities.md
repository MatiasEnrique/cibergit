# Check identities

The Checks panel is a bounded, read-only view of the status contexts and check runs that GitHub returned for a pull request. It does not fetch logs, rerun jobs, cancel work, or change the pinned comparison.

## What is recorded

Each collaboration snapshot records the observed pull request node, base repository, head commit and head repository. It also records the status rollup commit and repository when a rollup exists, plus the optional merge-candidate commit and repository.

A check row keeps its opaque node ID, kind, name, status, conclusion, requiredness, observed commit, commit relation, and returned repository identity. Check runs can also carry their nullable numeric database ID, GitHub permalink, suite identity, app identity, and the suite's optional workflow-run relation. Commit statuses never gain CheckRun or workflow identity.

Opaque IDs stay opaque. Numeric GraphQL `Int` values are accepted only in the positive signed 32-bit range and then widened to `u64`. A missing, null, zero, negative, or out-of-range numeric ID remains unknown. The mapper never extracts a number from a node ID.

The query follows GitHub's current public GraphQL schema for [Checks](https://docs.github.com/en/graphql/reference/checks) and [Actions](https://docs.github.com/en/graphql/reference/actions). The schema was rechecked on 2026-09-14 before these fields were added.

## Workflow linkage

Only a complete returned `CheckSuite.workflowRun` tuple identifies a GitHub Actions run. The tuple includes the workflow-run node and database IDs, run attempt, run number, event, GitHub URL, workflow node and database IDs, and workflow name.

An explicit null relation is displayed as "No linked GitHub Actions run observed." An omitted, partial, or invalid relation is "GitHub Actions linkage unknown." A name, app slug, or URL that happens to mention Actions cannot change either result.

The "GitHub check permalink" is GitHub-owned identity. The "Integrator URL (display only)" comes from `detailsUrl` or `targetUrl`. Cibergit displays that value as text and does not fetch it or treat it as GitHub identity.

## Consistency and completeness

The provider freezes source identity across every requested page. A changed head, rollup commit, rollup repository, merge candidate, base repository, head repository, or pull request node rejects the paginated result instead of returning a complete-looking prefix.

Nested CheckRun, suite, and commit repositories must agree. A returned origin may match the base repository, the observed head repository for a fork, or the rollup repository. An unrelated repository is rejected. A status context's returned commit follows the same association rule.

An explicit null `statusCheckRollup` is a complete observation that no rollup was returned. Missing requested rollup data, missing or null requested `contexts`, null nodes, unknown node variants, GraphQL errors, invalid requested identity fields, and pagination limits make the snapshot incomplete. An empty terminal `contexts.nodes` array is complete. Once the Checks cursor is complete, later activity-only pages may omit `contexts` without changing Checks completeness.

The number of rows displayed on a page is separate from provider completeness. The panel renders at most 40 rows at once, but Previous 40 and Next 40 make every already-enumerated row reachable. It never renders an unbounded check list.

## Native controls

Use Shift-Command-C or the "Open Checks" command to open and focus the panel. Up and Down select the previous or next exact row. Page Up and Page Down move by a 40-row page. Enter expands or collapses the selected row. Pointer selection and keyboard selection use the same opaque check ID.

Selection and expansion follow the exact check ID after a refresh or reorder. If an identity disappears, selection falls back to the first returned check and stale expansion closes. Navigation asks GPUI to reveal the measured row, so an offscreen selection and its expanded metadata remain visible even when labels wrap.

## Cached observations

The collaboration cache stores these fields as historical evidence only. Legacy records without them deserialize as unknown. Current records validate source SHAs, repository associations, numeric bounds, URL roles, suite consistency, and workflow tuples before display. The cache does not create fresh permissions, write capability, or a new comparison revision.

## Deliberate limits

This slice adds no log transport and no Actions controls. A future log reader must prove redirect handling independently, must not forward authorization to signed storage or an arbitrary integrator URL, and must keep signed query strings out of evidence. Future rerun or cancellation controls need endpoint-specific server authority and acknowledgement rules. Viewer roles alone are not enough.
