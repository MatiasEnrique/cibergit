# Explicit PR source publishing

The local workspace can publish the attached branch to the source branch of the selected GitHub pull request. Publishing is always explicit. Rebase completion only refreshes the local inventory; it never pushes.

The published Review stays pinned to its selected revision. A later provider refresh may report a newer source head, but only the existing Review advance control changes the canonical comparison.

## Prepare and confirm

"Prepare publish" performs read-only checks and freezes one confirmation:

- the selected GitHub account, base repository, and pull request number;
- a fresh provider read of the source repository, source branch, target branch, and published head;
- the accepted checkout, Git directory, and common Git directory paths and device/inode identities;
- the attached local branch and its full object ID;
- one installed-Git remote whose effective push URL matches the provider source repository;
- the exact source branch object ID read from that effective push endpoint.

The local branch and published branch are separate values. A managed local branch such as `cibergit/local-42` may publish to `feature/topic`. cibergit does not infer this mapping from a hashed or historical local name.

The confirmation panel prints the full safe target, both branch names, the full local object ID, and the observed remote object ID. Refreshing the target replaces the preparation. Once confirmation is open, those values do not change.

## Installed Git decides the destination

cibergit runs `git remote get-url --all` and `git remote get-url --push --all`. Git expands `url.*.insteadOf` and `url.*.pushInsteadOf` for these commands. An explicit `pushurl` takes precedence over `pushInsteadOf`, as documented by Git.

The destination check accepts exactly one effective push URL for exactly one matching remote. It refuses these cases:

- no push destination matches the fresh source repository;
- more than one remote or push URL can target it;
- a URL has embedded HTTP credentials;
- a URL cannot be reduced to a credential-free host/owner/repository identity;
- the source repository is deleted or unavailable;
- the provider, selected account, repository, PR number, source branch, or head does not match.

The code keeps the effective push URL private. UI messages, debug output, journal records, and receipts contain only safe host/owner/repository labels and an opaque SHA-256 configuration fingerprint. cibergit never adds a remote, changes Git configuration, or changes authentication to make a target available.

For local-only tests and native smoke scenes, a provider fixture may name a canonical local bare repository. Production GitHub source observations use host/owner/repository identity.

References:

- [git-remote `get-url`](https://git-scm.com/docs/git-remote#Documentation/git-remote.txt-get-url)
- [Git `url.<base>.pushInsteadOf`](https://git-scm.com/docs/git-config#Documentation/git-config.txt-urlbasepushInsteadOf)
- [git-push exact force-with-lease](https://git-scm.com/docs/git-push#Documentation/git-push.txt---force-with-leaserefnameexpect)

## Publish versus re-publish

The preparation compares the observed provider head with the frozen local object ID.

- If the provider head is an ancestor of the local object ID, the button says "Publish". cibergit sends a normal, non-force push with the frozen local object ID as the refspec source. Git's server-side fast-forward rules remain authoritative.
- If the local history rewrote the published head, the button says "Re-publish with lease". cibergit sends `--force-with-lease=refs/heads/<source>:<observed-oid>` and the frozen local object ID.
- If both object IDs are equal, the panel says "Published" and does not offer a push.

After the journal reaches durable storage and before Git starts, cibergit reads the provider source again. It refuses if the account, PR, repository, branch, or head changed. While holding the common-Git-directory mutation lock, it also rechecks the effective push configuration, the remote branch object ID, the checkout snapshot guard, and the local branch object ID.

An exact lease protects only the named remote ref. A normal push has normal Git fast-forward behavior. The read immediately before a normal push is not a server-side compare-and-swap, so a remote change in the small gap before dispatch relies on the server's fast-forward rejection. External processes do not share cibergit's process-local Git lock.

## Durable attempts and reconciliation

PR source pushes use the existing private `started-local-action.json` journal. The record reaches durable storage before Git dispatch and contains the request and attempt IDs, safe selected and source identities, both branch names and object IDs, the mode, filesystem identities, destination label, and opaque configuration fingerprint.

A refusal before Git starts clears only the exact matching journal record. If Git starts or the result becomes uncertain, cibergit keeps the record. Restart and reconciliation perform provider, effective push-endpoint, remote-ref, and local Git reads. They never replay the push. A missing remote ref is shown as missing; it is not treated as proof that a prior attempt did not apply.

Local publish preparation and confirmation share the checkout-effect lane with branch changes and rebase. They pause for an active rebase transition, merge/cherry-pick/revert state, document open, unsaved editor text, queued document work, unresolved recovery, another confirmation, or durable reconciliation. Refresh keeps dirty document buffers intact.

## Local verification

The focused fixtures use temporary repositories and fixed fake-provider observations. They cover fork source versus base repository, distinct local and remote branches, fetch URL versus push URL, `pushInsteadOf`, multiple push URLs, embedded credentials, provider identity changes, deleted source, destination changes after confirmation, moved leases, local-ref races, durable admission failure, retained restart state, and an actual confirmed push to a local bare repository.

The native example keeps `focus: false`, does not call `cx.activate()`, and uses disposable data. Its dark and light wide/narrow captures show the complete target and frozen confirmation without contacting GitHub.
