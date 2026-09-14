# Stack view

Open a pull request, then choose **Stack** or press `Shift Command S`. The view compares one proven effective base with one explicitly selected tip. It does not add the patches from each pull request together.

The left column lists each layer and labels its relationship as Native, Inferred, or Personal. Native data comes from GitHub's read-only GraphQL stack fields. If native membership is unsupported, partial, or capped, the notice stays visible while cibergit tries branch-based inference. Candidate inference reads all pull requests in the selected repository, including merged lower layers. It accepts at most 1,000 candidates and repeats the complete read before using it.

The Stack view refuses moving, partial, capped, cyclic, or incomplete relationships. An all-merged stack says that every layer is merged instead of showing an empty successful diff.

## Choosing a tip

A pull request can have more than one stack built on top of it. cibergit never guesses which one you meant.

**Tips below this PR** lists every selectable tip by number and branch, with its state, the number of layers on its path, and its head. Only tips that descend from the pull request that opened Stack are listed, so the pull request you opened is always on the path you are shown. Sibling branches that do not contain it are not offered. Choose a tip, or press `Option Command T` to cycle, and Stack compares one proven effective base with that tip and lists only the layers on its path.

Until you choose, no path and no comparison are shown. A stack with exactly one tip is compared immediately, as before.

A tip is offered only when its path can be proven: exactly one parent per layer, no cycle. A leaf that fails this is withheld and its exact reason stays visible under the list, so a short list never implies that the rest of the graph is valid. cibergit never synthesizes a merge and never chooses among ambiguous parents.

Your choice is remembered as an exact pull request identity, not a position, so a refresh that reorders the read keeps it. If the chosen tip is no longer a candidate, Stack says so, clears the comparison, and asks you to choose again. It does not substitute another tip, even when only one remains.

Choosing a tip supersedes every outstanding read. A reply for a previously selected tip, an older refresh, or a lazy patch load started against the previous pair is ignored rather than mixed into the rendered comparison.

## Personal relationships

Choose **Edit personal relationship** in the layer column to label another pull request as the parent of the pull request that opened Stack. The editor stays closed until requested. At narrow widths, choose **Layers** or press `Option Command L` to replace the diff temporarily with the bounded layer panel; use the same control to return. The selected layer’s provenance remains in the header either way. A correction changes only a local Personal relationship. It does not edit a GitHub base branch.

Each account and repository has a separate versioned JSON record. cibergit locks the record across processes and uses compare-and-swap when it writes. A stale window cannot replace a newer correction. Corrupt records and records from a future schema remain untouched. Use **Remove this personal parent** for one correction or **Reset all personal relationships** for the repository partition.

The corrected graph must still pass normal stack resolution before cibergit saves it. Foreign pull requests, account mismatches, repeated parents, cycles, and more than 1,000 links fail closed.

## Refresh and review behavior

Stack owns its comparison, file selection, diff mode, and scroll positions. Opening or refreshing it never replaces the pinned pull request comparison, canonical review revision, review draft, or local checkout.

If published code moves, the existing Stack pair stays on screen and reports the newer head. Choose **Refresh Stack** or press `Option Command R` to read and prove a new pair. Replies from an older refresh, a closed view, a prior tab instance, another account, or a superseded correction are ignored.

The changed-file list and source rows are virtualized. Only the selected local file can load an unavailable patch, and that load uses the exact effective-base-to-tip revision. Binary and media files remain metadata only. Unified and side-by-side views keep OLD and NEW identities, and long lines scroll horizontally to their measured end.

Comments, review submission, merge, and descendant repair stay in the ordinary pull request view. Press `Escape` or choose **Pull request** to return. The Stack view has no aggregate write action.

## Provider fields and bounds

GitHub documents `PullRequest.stack` and `stackEntry` as read-only GraphQL fields. Candidate inference uses the REST list-pull-requests response fields `number`, `state`, `merged_at`, `merge_commit_sha`, `html_url`, `base`, and `head`. REST pages contain at most 100 items. cibergit reads no more than 10 pages and never treats a full final page as proof that the list ended.

Primary references:

- [GitHub stacked pull request APIs and webhooks](https://docs.github.com/en/pull-requests/reference/stacked-pull-requests-apis-and-webhooks)
- [GitHub REST pull request endpoints](https://docs.github.com/en/rest/pulls/pulls)
- [GitHub GraphQL pull request objects](https://docs.github.com/en/graphql/reference/objects)
