//! Explicit PR-source publication preparation.
//!
//! Provider state is read only after an explicit prepare/refresh gesture. The
//! resulting value is immutable and safe to show in confirmation UI; it never
//! changes the caller's canonical published Review revision.

use super::validate_checkout;
use cibergit::{
    domain::{PullRequestCheckoutSource, Repository},
    local_git::{
        GitRepositoryIdentity, GitRepositoryTarget, HeadState, LocalGit, MutationReceipt,
        OperationState, PushDestinationObservation, SnapshotGuard,
    },
    providers::GithubProvider,
    worktrees::{CheckoutView, FilesystemIdentity},
};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

trait PrSourceReader: Send + Sync {
    fn checkout_source(
        &self,
        repository: &Repository,
        number: u64,
    ) -> Result<PullRequestCheckoutSource, String>;
}

struct GithubPrSourceReader;

impl PrSourceReader for GithubPrSourceReader {
    fn checkout_source(
        &self,
        repository: &Repository,
        number: u64,
    ) -> Result<PullRequestCheckoutSource, String> {
        GithubProvider::new(repository.account.clone())
            .checkout_source(repository, number)
            .map_err(|error| format!("fresh PR source read failed: {error:#}"))
    }
}

#[cfg(feature = "ui-smoke")]
struct FixedPrSourceReader(PullRequestCheckoutSource);

#[cfg(feature = "ui-smoke")]
impl PrSourceReader for FixedPrSourceReader {
    fn checkout_source(
        &self,
        _repository: &Repository,
        _number: u64,
    ) -> Result<PullRequestCheckoutSource, String> {
        Ok(self.0.clone())
    }
}

/// Selected provider/account/PR identity installed after LocalWorkspace
/// construction. The constructor remains stable for non-PR workspaces.
#[derive(Clone)]
pub struct PrPublishContext {
    selected_repository: Repository,
    pull_request_number: u64,
    reader: Arc<dyn PrSourceReader>,
}

impl fmt::Debug for PrPublishContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrPublishContext")
            .field("host", &self.selected_repository.host)
            .field("repository", &self.selected_repository.full_name())
            .field("account", &self.selected_repository.account.login)
            .field("pull_request_number", &self.pull_request_number)
            .finish_non_exhaustive()
    }
}

impl PrPublishContext {
    pub fn github(selected_repository: Repository, pull_request_number: u64) -> Self {
        Self {
            selected_repository,
            pull_request_number,
            reader: Arc::new(GithubPrSourceReader),
        }
    }

    /// Disposable native evidence only: supplies a fixed read-only provider
    /// observation so the smoke never contacts or writes GitHub.
    #[cfg(feature = "ui-smoke")]
    pub fn fixed_for_smoke(
        selected_repository: Repository,
        pull_request_number: u64,
        source: PullRequestCheckoutSource,
    ) -> Self {
        Self {
            selected_repository,
            pull_request_number,
            reader: Arc::new(FixedPrSourceReader(source)),
        }
    }

    #[cfg(test)]
    fn with_reader(
        selected_repository: Repository,
        pull_request_number: u64,
        reader: Arc<dyn PrSourceReader>,
    ) -> Self {
        Self {
            selected_repository,
            pull_request_number,
            reader,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrPublishMode {
    UpToDate,
    Publish,
    RepublishWithLease,
}

impl PrPublishMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::UpToDate => "Published",
            Self::Publish => "Publish",
            Self::RepublishWithLease => "Re-publish with lease",
        }
    }
}

/// Filesystem identities accepted both by WorktreeManager and fresh installed
/// Git. Paths are not serialized into the durable attempt record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrPublishFilesystemIdentity {
    pub checkout: FilesystemIdentity,
    pub git_dir: FilesystemIdentity,
    pub common_git_dir: FilesystemIdentity,
}

#[derive(Clone)]
pub struct PrPublishPreparation {
    pub selected_host: String,
    pub selected_account: String,
    pub selected_repository: String,
    pub pull_request_number: u64,
    pub source_host: String,
    pub source_repository: String,
    pub local_branch: String,
    pub local_oid: String,
    pub remote_branch: String,
    pub provider_head_oid: String,
    pub expected_remote_oid: String,
    pub destination: PushDestinationObservation,
    pub filesystem: PrPublishFilesystemIdentity,
    pub mode: PrPublishMode,
    snapshot_guard: SnapshotGuard,
    selected_repository_identity: Repository,
    prepared_source: PullRequestCheckoutSource,
    reader: Arc<dyn PrSourceReader>,
}

impl fmt::Debug for PrPublishPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrPublishPreparation")
            .field("selected_host", &self.selected_host)
            .field("selected_account", &self.selected_account)
            .field("selected_repository", &self.selected_repository)
            .field("pull_request_number", &self.pull_request_number)
            .field("source_host", &self.source_host)
            .field("source_repository", &self.source_repository)
            .field("local_branch", &self.local_branch)
            .field("local_oid", &self.local_oid)
            .field("remote_branch", &self.remote_branch)
            .field("provider_head_oid", &self.provider_head_oid)
            .field("expected_remote_oid", &self.expected_remote_oid)
            .field("destination", &self.destination)
            .field("filesystem", &self.filesystem)
            .field("mode", &self.mode)
            .field("snapshot_guard", &"<opaque>")
            .field("provider_revalidation", &"<credential-withholding reader>")
            .finish()
    }
}

impl PrPublishPreparation {
    pub fn summary(&self) -> String {
        match self.mode {
            PrPublishMode::UpToDate => format!(
                "Published {} from local {} at {} on {}/{}",
                self.source_repository,
                self.local_branch,
                short_oid(&self.local_oid),
                self.destination.remote,
                self.remote_branch,
            ),
            PrPublishMode::Publish => format!(
                "Publish {} at {} from local {} at {} to {}/{} (non-force)",
                self.source_repository,
                short_oid(&self.provider_head_oid),
                self.local_branch,
                short_oid(&self.local_oid),
                self.destination.remote,
                self.remote_branch,
            ),
            PrPublishMode::RepublishWithLease => format!(
                "Re-publish {} from local {} at {} to {}/{} with exact lease {}",
                self.source_repository,
                self.local_branch,
                short_oid(&self.local_oid),
                self.destination.remote,
                self.remote_branch,
                short_oid(&self.expected_remote_oid),
            ),
        }
    }

    pub fn snapshot_guard(&self) -> &SnapshotGuard {
        &self.snapshot_guard
    }

    pub fn attempt(&self, request_id: u64) -> PrPublishAttempt {
        PrPublishAttempt {
            request_id,
            attempt_id: request_id,
            selected_host: self.selected_host.clone(),
            selected_account: self.selected_account.clone(),
            selected_repository: self.selected_repository.clone(),
            pull_request_number: self.pull_request_number,
            source_host: self.source_host.clone(),
            source_repository: self.source_repository.clone(),
            local_branch: self.local_branch.clone(),
            local_oid: self.local_oid.clone(),
            destination_remote: self.destination.remote.clone(),
            destination_repository: format!(
                "{}/{}/{}",
                self.destination.repository.host,
                self.destination.repository.owner,
                self.destination.repository.name
            ),
            destination_configuration_fingerprint: self.destination.configuration_fingerprint(),
            remote_branch: self.remote_branch.clone(),
            provider_head_oid: self.provider_head_oid.clone(),
            expected_remote_oid: self.expected_remote_oid.clone(),
            filesystem: self.filesystem,
            mode: self.mode,
        }
    }

    pub fn dispatch(
        &self,
        git: &LocalGit,
        guard: &SnapshotGuard,
    ) -> cibergit::local_git::Result<MutationReceipt> {
        let fresh = self
            .reader
            .checkout_source(&self.selected_repository_identity, self.pull_request_number)
            .map_err(|_| {
                cibergit::local_git::LocalGitError::InvalidInput(
                    "fresh PR source revalidation failed before Git dispatch",
                )
            })?;
        if fresh != self.prepared_source {
            return Err(cibergit::local_git::LocalGitError::InvalidInput(
                "fresh PR source/account/repository/branch/head changed since confirmation",
            ));
        }
        match self.mode {
            PrPublishMode::UpToDate => Err(cibergit::local_git::LocalGitError::InvalidInput(
                "the PR source branch already has the prepared local OID",
            )),
            PrPublishMode::Publish => git.push_branch_to_destination(
                &self.destination,
                &self.local_branch,
                &self.local_oid,
                &self.remote_branch,
                &self.expected_remote_oid,
                guard,
            ),
            PrPublishMode::RepublishWithLease => git.force_push_branch_with_lease_to_destination(
                &self.destination,
                &self.local_branch,
                &self.local_oid,
                &self.remote_branch,
                &self.expected_remote_oid,
                guard,
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrPublishAttempt {
    pub request_id: u64,
    pub attempt_id: u64,
    pub selected_host: String,
    pub selected_account: String,
    pub selected_repository: String,
    pub pull_request_number: u64,
    pub source_host: String,
    pub source_repository: String,
    pub local_branch: String,
    pub local_oid: String,
    pub destination_remote: String,
    pub destination_repository: String,
    pub destination_configuration_fingerprint: String,
    pub remote_branch: String,
    pub provider_head_oid: String,
    pub expected_remote_oid: String,
    pub filesystem: PrPublishFilesystemIdentity,
    pub mode: PrPublishMode,
}

pub fn prepare(
    context: &PrPublishContext,
    checkout: &CheckoutView,
    git: &LocalGit,
) -> Result<PrPublishPreparation, String> {
    let source = context
        .reader
        .checkout_source(&context.selected_repository, context.pull_request_number)?;
    prepare_with_source(context, checkout, git, source)
}

fn prepare_with_source(
    context: &PrPublishContext,
    checkout: &CheckoutView,
    git: &LocalGit,
    source: PullRequestCheckoutSource,
) -> Result<PrPublishPreparation, String> {
    validate_selected_identity(context, checkout, &source)?;
    validate_checkout(checkout, git)?;
    let source_repository = source.source_repository.clone().ok_or_else(|| {
        "The PR source repository was deleted or is unavailable; the target repository will not be used as a fallback"
            .to_owned()
    })?;
    if !source_repository
        .host
        .eq_ignore_ascii_case(&context.selected_repository.host)
        || !source_repository
            .account
            .host
            .eq_ignore_ascii_case(&source_repository.host)
        || !same_account(&source_repository, &context.selected_repository)
    {
        return Err("Fresh PR source account does not match the selected account".into());
    }
    let snapshot = git.snapshot().map_err(|error| error.to_string())?;
    if snapshot.operation != OperationState::default() {
        return Err(
            "Publishing is paused while a merge, rebase, cherry-pick, or revert is active".into(),
        );
    }
    let (local_branch, local_oid) = match &snapshot.head {
        HeadState::Attached { branch, oid } => (branch.clone(), oid.clone()),
        HeadState::Detached { .. } => {
            return Err("Attach a local branch before preparing PR publication".into());
        }
        HeadState::Unborn { .. } => {
            return Err("The attached local branch has no commit to publish".into());
        }
    };
    if snapshot.local_branch_oids.get(&local_branch) != Some(&local_oid) {
        return Err("Attached local branch identity changed during preparation".into());
    }
    let destination = git
        .observe_push_destination(&GitRepositoryTarget {
            repository: GitRepositoryIdentity {
                host: source_repository.host.clone(),
                owner: source_repository.owner.clone(),
                name: source_repository.name.clone(),
            },
            local_path: source_repository.local_path.clone(),
        })
        .map_err(|error| error.to_string())?;
    let remote = git
        .observe_destination_branch(&destination, &source.source_branch)
        .map_err(|error| error.to_string())?;
    let expected_remote_oid = remote.oid.ok_or_else(|| {
        "The configured push endpoint does not contain the fresh PR source branch; no absent state was inferred"
            .to_owned()
    })?;
    if expected_remote_oid != source.observed_revision.head_sha {
        return Err(
            "Provider and installed Git report different PR source heads; refresh before publishing"
                .into(),
        );
    }
    let mode = if local_oid == expected_remote_oid {
        PrPublishMode::UpToDate
    } else if git
        .commit_is_ancestor(&expected_remote_oid, &local_oid)
        .map_err(|error| error.to_string())?
    {
        PrPublishMode::Publish
    } else {
        PrPublishMode::RepublishWithLease
    };
    Ok(PrPublishPreparation {
        selected_host: context.selected_repository.host.clone(),
        selected_account: context.selected_repository.account.login.clone(),
        selected_repository: context.selected_repository.full_name(),
        pull_request_number: context.pull_request_number,
        source_host: source_repository.host.clone(),
        source_repository: source_repository.full_name(),
        local_branch,
        local_oid,
        remote_branch: source.source_branch.clone(),
        provider_head_oid: source.observed_revision.head_sha.clone(),
        expected_remote_oid,
        destination,
        filesystem: PrPublishFilesystemIdentity {
            checkout: checkout.association.checkout_identity,
            git_dir: checkout.association.git_dir_identity,
            common_git_dir: checkout.association.common_git_dir_identity,
        },
        mode,
        snapshot_guard: snapshot.guard,
        selected_repository_identity: context.selected_repository.clone(),
        prepared_source: source,
        reader: context.reader.clone(),
    })
}

#[cfg(test)]
struct TestPrSourceReader(PullRequestCheckoutSource);

#[cfg(test)]
impl PrSourceReader for TestPrSourceReader {
    fn checkout_source(
        &self,
        _repository: &Repository,
        _number: u64,
    ) -> Result<PullRequestCheckoutSource, String> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
pub(super) fn prepare_test_source(
    selected_repository: Repository,
    pull_request_number: u64,
    checkout: &CheckoutView,
    git: &LocalGit,
    source: PullRequestCheckoutSource,
) -> Result<PrPublishPreparation, String> {
    let reader_source = source.clone();
    let context = PrPublishContext::with_reader(
        selected_repository,
        pull_request_number,
        Arc::new(TestPrSourceReader(reader_source)),
    );
    prepare_with_source(&context, checkout, git, source)
}

fn validate_selected_identity(
    context: &PrPublishContext,
    checkout: &CheckoutView,
    source: &PullRequestCheckoutSource,
) -> Result<(), String> {
    if context.pull_request_number == 0
        || !context
            .selected_repository
            .account
            .host
            .eq_ignore_ascii_case(&context.selected_repository.host)
        || source.number != context.pull_request_number
        || !same_repository(&source.base_repository, &context.selected_repository)
    {
        return Err("Fresh PR source does not match the selected repository and PR".into());
    }
    let key = &checkout.association.key;
    if key.provider != "github"
        || !key
            .host
            .eq_ignore_ascii_case(&context.selected_repository.host)
        || key.account != context.selected_repository.account.login
        || !key
            .repository
            .eq_ignore_ascii_case(&context.selected_repository.full_name())
        || key.pull_request != context.pull_request_number
    {
        return Err(
            "Attached checkout identity does not match the selected account/repository/PR".into(),
        );
    }
    Ok(())
}

fn same_repository(left: &Repository, right: &Repository) -> bool {
    left.host.eq_ignore_ascii_case(&right.host)
        && left.owner.eq_ignore_ascii_case(&right.owner)
        && left.name.eq_ignore_ascii_case(&right.name)
        && same_account(left, right)
}

fn same_account(left: &Repository, right: &Repository) -> bool {
    left.account.host.eq_ignore_ascii_case(&right.account.host)
        && left.account.login == right.account.login
}

fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::{
        domain::{Account, Revision},
        worktrees::{AssociationKey, CheckoutAssociation, CheckoutOwnership},
    };
    use std::{collections::VecDeque, fs, path::Path, process::Command, sync::Mutex};
    use tempfile::TempDir;

    struct FixedReader(Result<PullRequestCheckoutSource, String>);

    impl PrSourceReader for FixedReader {
        fn checkout_source(
            &self,
            _repository: &Repository,
            _number: u64,
        ) -> Result<PullRequestCheckoutSource, String> {
            self.0.clone()
        }
    }

    struct SequenceReader(Mutex<VecDeque<PullRequestCheckoutSource>>);

    impl PrSourceReader for SequenceReader {
        fn checkout_source(
            &self,
            _repository: &Repository,
            _number: u64,
        ) -> Result<PullRequestCheckoutSource, String> {
            self.0
                .lock()
                .expect("reader lock")
                .pop_front()
                .ok_or_else(|| "no source observation".into())
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 output")
            .trim()
            .into()
    }

    fn fixture() -> (
        TempDir,
        TempDir,
        PrPublishContext,
        CheckoutView,
        LocalGit,
        PullRequestCheckoutSource,
        String,
    ) {
        let checkout = TempDir::new().expect("checkout tempdir");
        let source_bare = TempDir::new().expect("source tempdir");
        git(checkout.path(), &["init", "-q", "-b", "cibergit/local-7"]);
        git(checkout.path(), &["config", "user.name", "Test"]);
        git(
            checkout.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(checkout.path().join("file"), "published\n").expect("file");
        git(checkout.path(), &["add", "."]);
        git(checkout.path(), &["commit", "-qm", "published"]);
        let published = git(checkout.path(), &["rev-parse", "HEAD"]);
        git(source_bare.path(), &["init", "--bare", "-q"]);
        git(
            checkout.path(),
            &[
                "remote",
                "add",
                "fork-source",
                source_bare.path().to_str().expect("path"),
            ],
        );
        git(
            checkout.path(),
            &[
                "push",
                "-q",
                "fork-source",
                "cibergit/local-7:refs/heads/feature/published",
            ],
        );
        fs::write(checkout.path().join("file"), "next\n").expect("file");
        git(checkout.path(), &["add", "."]);
        git(checkout.path(), &["commit", "-qm", "next"]);
        let local = LocalGit::open(checkout.path()).expect("local git");
        let snapshot = local.snapshot().expect("snapshot");
        let selected = Repository {
            host: "github.test".into(),
            owner: "base-owner".into(),
            name: "base-repo".into(),
            account: Account {
                host: "github.test".into(),
                login: "selected".into(),
            },
            local_path: Some(local.root().to_owned()),
        };
        let source_repository = Repository {
            host: "github.test".into(),
            owner: "fork-owner".into(),
            name: "source-repo".into(),
            account: selected.account.clone(),
            local_path: Some(source_bare.path().to_owned()),
        };
        let source = PullRequestCheckoutSource {
            number: 7,
            base_repository: selected.clone(),
            source_repository: Some(source_repository),
            source_branch: "feature/published".into(),
            target_branch: "main".into(),
            observed_revision: Revision {
                base_sha: published.clone(),
                head_sha: published.clone(),
            },
        };
        let checkout_view = CheckoutView {
            association: CheckoutAssociation {
                key: AssociationKey {
                    provider: "github".into(),
                    host: "github.test".into(),
                    account: "selected".into(),
                    repository: "base-owner/base-repo".into(),
                    pull_request: 7,
                },
                path: local.root().to_owned(),
                git_dir: local.git_dir().to_owned(),
                common_git_dir: local.common_git_dir().to_owned(),
                checkout_identity: super::super::filesystem_identity(local.root())
                    .expect("identity"),
                git_dir_identity: super::super::filesystem_identity(local.git_dir())
                    .expect("identity"),
                common_git_dir_identity: super::super::filesystem_identity(local.common_git_dir())
                    .expect("identity"),
                ownership: CheckoutOwnership::ExplicitlyAttached,
                creation: None,
                intended_remote_branch: Some("historical-do-not-trust".into()),
                published_head_at_association: Some("historical-do-not-trust".into()),
            },
            actual_head: snapshot.head,
            operation: snapshot.operation,
        };
        let context =
            PrPublishContext::with_reader(selected, 7, Arc::new(FixedReader(Ok(source.clone()))));
        (
            checkout,
            source_bare,
            context,
            checkout_view,
            local,
            source,
            published,
        )
    }

    #[test]
    fn context_debug_and_attempt_never_contain_provider_transport_urls() {
        let repository = Repository {
            host: "github.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
            account: cibergit::domain::Account {
                host: "github.test".into(),
                login: "selected".into(),
            },
            local_path: None,
        };
        let context = PrPublishContext::with_reader(
            repository,
            7,
            Arc::new(FixedReader(Err("read refused".into()))),
        );
        let debug = format!("{context:?}");
        assert!(debug.contains("owner/repo"));
        assert!(!debug.contains("http"));
    }

    #[test]
    fn fake_provider_fork_preparation_freezes_distinct_local_and_remote_branches() {
        let (_checkout, source_bare, context, checkout, git, _source, published) = fixture();
        let prepared = prepare(&context, &checkout, &git).expect("prepare");
        assert_eq!(prepared.mode, PrPublishMode::Publish);
        assert_eq!(prepared.selected_repository, "base-owner/base-repo");
        assert_eq!(prepared.source_repository, "fork-owner/source-repo");
        assert_eq!(prepared.local_branch, "cibergit/local-7");
        assert_eq!(prepared.remote_branch, "feature/published");
        assert_eq!(prepared.expected_remote_oid, published);
        assert_ne!(prepared.local_oid, prepared.expected_remote_oid);
        assert_eq!(prepared.destination.remote, "fork-source");
        let attempt = prepared.attempt(44);
        assert_eq!(attempt.request_id, 44);
        assert_eq!(attempt.attempt_id, 44);
        assert_eq!(attempt.destination_configuration_fingerprint.len(), 64);
        assert!(!format!("{attempt:?}").contains(&source_bare.path().display().to_string()));
    }

    #[test]
    fn rewritten_local_history_requires_explicit_republish_lease_mode() {
        let (_checkout, _source_bare, context, checkout, backend, _source, published) = fixture();
        let tree = git(backend.root(), &["rev-parse", "HEAD^{tree}"]);
        let rewritten = git(backend.root(), &["commit-tree", &tree, "-m", "rewritten"]);
        git(
            backend.root(),
            &["update-ref", "refs/heads/cibergit/local-7", &rewritten],
        );
        git(backend.root(), &["reset", "--hard", "-q", &rewritten]);
        let prepared = prepare(&context, &checkout, &backend).expect("prepare rewritten");
        assert_eq!(prepared.mode, PrPublishMode::RepublishWithLease);
        assert_eq!(prepared.expected_remote_oid, published);
        assert_eq!(prepared.local_oid, rewritten);
    }

    #[test]
    fn selected_identity_mismatch_and_deleted_source_never_fall_back_to_base() {
        let (_checkout, _source_bare, context, checkout, git, source, _published) = fixture();
        let mut mismatch = source.clone();
        mismatch.number = 8;
        assert!(
            prepare_with_source(&context, &checkout, &git, mismatch)
                .unwrap_err()
                .contains("selected repository and PR")
        );
        let mut mismatch = source.clone();
        mismatch
            .source_repository
            .as_mut()
            .expect("source")
            .account
            .login = "other".into();
        assert!(
            prepare_with_source(&context, &checkout, &git, mismatch)
                .unwrap_err()
                .contains("selected account")
        );
        let mut deleted = source;
        deleted.source_repository = None;
        let error = prepare_with_source(&context, &checkout, &git, deleted).unwrap_err();
        assert!(error.contains("deleted or is unavailable"));
        assert!(error.contains("will not be used as a fallback"));
    }

    #[test]
    fn provider_source_change_between_prepare_and_dispatch_refuses_before_git() {
        let (_checkout, source_bare, _context, checkout, backend, source, published) = fixture();
        let selected = source.base_repository.clone();
        let mut changed = source.clone();
        changed.source_branch = "different/provider-branch".into();
        let context = PrPublishContext::with_reader(
            selected,
            7,
            Arc::new(SequenceReader(Mutex::new(VecDeque::from([
                source, changed,
            ])))),
        );
        let prepared = prepare(&context, &checkout, &backend).expect("first fresh read");
        let error = prepared
            .dispatch(&backend, prepared.snapshot_guard())
            .expect_err("second fresh read must refuse");
        assert!(matches!(
            error,
            cibergit::local_git::LocalGitError::InvalidInput(_)
        ));
        assert_eq!(
            git(
                source_bare.path(),
                &["rev-parse", "refs/heads/feature/published"]
            ),
            published
        );
    }
}
