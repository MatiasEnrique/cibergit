//! Immutable full-PR, commit, range, and since-review selection.
//!
//! The canonical full PR revision remains separate from the pair displayed by a
//! narrower comparison. Call these synchronous readers away from the UI thread.

use crate::{
    domain::{Account, Comparison, PullRequestReview, Repository, Revision},
    providers::GithubProvider,
    review::{
        ComparisonMetadata, ComparisonMode, local_comparison, local_inventory, local_pr_comparison,
        local_pr_inventory, validate_object_id,
    },
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsStr,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub const MAX_COMMIT_INVENTORY: usize = 1_000;
const MAX_LOCAL_INVENTORY_BYTES: usize = 4 * 1024 * 1024;
const GIT_READ_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInventoryEntry {
    pub sha: String,
    pub parent_shas: Vec<String>,
    pub message_headline: String,
    pub authored_at: String,
    pub committed_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryAvailability {
    Complete,
    Incomplete,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInventory {
    pub full_revision: Revision,
    pub commits: Vec<CommitInventoryEntry>,
    pub availability: InventoryAvailability,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalReviewCompletion {
    /// Stable durable completion/submission record identity.
    pub completion_id: String,
    pub repository_key: String,
    pub account: Account,
    pub pull_request: u64,
    pub reviewed_head_sha: String,
    /// Persist normalized GitHub-style UTC timestamps so lexical ordering is stable.
    pub completed_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewBaselineSource {
    SubmittedReview { review_id: String },
    AcceptedLocalCompletion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBaseline {
    pub reviewed_head_sha: String,
    pub completed_at: String,
    pub source: ReviewBaselineSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaselineUnavailableReason {
    ReviewActivityIncomplete,
    NoSubmittedReview,
    LatestSubmittedReviewHasNoCommit,
    InvalidAcceptedLocalCompletion,
}

impl BaselineUnavailableReason {
    pub fn notice(&self) -> &'static str {
        match self {
            Self::ReviewActivityIncomplete => {
                "Review activity is incomplete, so the selected account's last submitted review cannot be established."
            }
            Self::NoSubmittedReview => {
                "The selected account has no submitted review or accepted local review completion."
            }
            Self::LatestSubmittedReviewHasNoCommit => {
                "The selected account's latest submitted review has no usable immutable commit."
            }
            Self::InvalidAcceptedLocalCompletion => {
                "The accepted local review completion has no usable immutable commit."
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BaselineResolution {
    Found(ReviewBaseline),
    Unavailable(BaselineUnavailableReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonRequest {
    FullPullRequest,
    Commit { sha: String },
    CommitRange { first_sha: String, last_sha: String },
    SinceLastReview { baseline: BaselineResolution },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFileLoadPlan {
    pub revision: Revision,
    /// Pass unchanged to `review::load_local_file`.
    pub full_pr: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComparisonSelection {
    /// Canonical published PR identity for progress and comment mapping.
    pub full_revision: Revision,
    /// The original request remains visible even when the service falls back.
    pub request: ComparisonRequest,
    pub comparison: Comparison,
    pub metadata: ComparisonMetadata,
    pub local_file_load: Option<LocalFileLoadPlan>,
}

/// Resolve only the selected repository account's last explicit completion.
/// Pending, other-account, other-PR, and merely-viewed state never qualify.
pub fn resolve_review_baseline(
    repo: &Repository,
    number: u64,
    reviews: &[PullRequestReview],
    reviews_complete: bool,
    local_completion: Option<&LocalReviewCompletion>,
) -> BaselineResolution {
    if !reviews_complete {
        return BaselineResolution::Unavailable(
            BaselineUnavailableReason::ReviewActivityIncomplete,
        );
    }

    let latest_remote = reviews
        .iter()
        .filter(|review| {
            review.state != "PENDING"
                && review.submitted_at.is_some()
                && review
                    .author
                    .as_deref()
                    .is_some_and(|author| author.eq_ignore_ascii_case(&repo.account.login))
                && review.coordinates.provider == "github"
                && review.coordinates.host == repo.host
                && review.coordinates.owner.eq_ignore_ascii_case(&repo.owner)
                && review
                    .coordinates
                    .repository
                    .eq_ignore_ascii_case(&repo.name)
                && review.coordinates.pull_request == number
        })
        .max_by_key(|review| review.submitted_at.as_deref().unwrap_or_default());

    let local = local_completion.filter(|completion| {
        completion.repository_key == repo.cache_key()
            && completion.account == repo.account
            && completion.pull_request == number
    });

    if let Some(completion) = local
        && (completion.completion_id.is_empty()
            || !completion.completed_at.ends_with('Z')
            || validate_object_id(&completion.reviewed_head_sha).is_err())
    {
        return BaselineResolution::Unavailable(
            BaselineUnavailableReason::InvalidAcceptedLocalCompletion,
        );
    }

    let use_local = local.is_some_and(|completion| {
        latest_remote.is_none_or(|review| {
            completion.completed_at.as_str() > review.submitted_at.as_deref().unwrap_or_default()
        })
    });
    if use_local {
        let completion = local.expect("checked above");
        return BaselineResolution::Found(ReviewBaseline {
            reviewed_head_sha: completion.reviewed_head_sha.clone(),
            completed_at: completion.completed_at.clone(),
            source: ReviewBaselineSource::AcceptedLocalCompletion,
        });
    }
    if let Some(review) = latest_remote {
        let Some(commit_sha) = review
            .commit_sha
            .as_ref()
            .filter(|sha| validate_object_id(sha).is_ok())
        else {
            return BaselineResolution::Unavailable(
                BaselineUnavailableReason::LatestSubmittedReviewHasNoCommit,
            );
        };
        return BaselineResolution::Found(ReviewBaseline {
            reviewed_head_sha: commit_sha.clone(),
            completed_at: review.submitted_at.clone().expect("filtered above"),
            source: ReviewBaselineSource::SubmittedReview {
                review_id: review.coordinates.remote_id.clone(),
            },
        });
    }
    BaselineResolution::Unavailable(BaselineUnavailableReason::NoSubmittedReview)
}

/// Enumerate commits reachable from the pinned head but not its one unambiguous
/// merge-base with the pinned target. No branch or worktree state is consulted.
pub fn local_commit_inventory(path: &Path, full_revision: &Revision) -> Result<CommitInventory> {
    validate_local_commit(path, &full_revision.base_sha)?;
    validate_local_commit(path, &full_revision.head_sha)?;
    let merge_bases = git(
        path,
        &[
            "merge-base",
            "--all",
            &full_revision.base_sha,
            &full_revision.head_sha,
        ],
    )?;
    let merge_bases: Vec<_> = std::str::from_utf8(&merge_bases)?
        .split_whitespace()
        .collect();
    ensure!(
        merge_bases.len() == 1,
        "Commit inventory needs one unambiguous merge-base"
    );
    validate_object_id(merge_bases[0])?;
    let range = format!("{}..{}", merge_bases[0], full_revision.head_sha);
    let count = git(path, &["rev-list", "--count", &range])?;
    let count: usize = std::str::from_utf8(&count)?.trim().parse()?;
    if count > MAX_COMMIT_INVENTORY {
        return Ok(CommitInventory {
            full_revision: full_revision.clone(),
            commits: Vec::new(),
            availability: InventoryAvailability::Incomplete,
            notice: Some(format!(
                "Commit inventory exceeds the {MAX_COMMIT_INVENTORY}-commit local limit."
            )),
        });
    }
    let output = git(
        path,
        &[
            "log",
            "-z",
            "--reverse",
            "--topo-order",
            "--format=%H%x00%P%x00%s%x00%aI%x00%cI",
            &range,
        ],
    )?;
    let fields: Vec<_> = output.split(|byte| *byte == 0).collect();
    ensure!(
        fields.last().is_none_or(|field| field.is_empty()) && (fields.len() - 1) % 5 == 0,
        "Invalid local commit inventory records"
    );
    let mut commits = Vec::with_capacity(count);
    let mut seen = HashSet::new();
    for record in fields[..fields.len().saturating_sub(1)].chunks_exact(5) {
        let sha = text_field(record[0], "commit OID")?;
        validate_object_id(&sha)?;
        ensure!(seen.insert(sha.clone()), "Repeated local commit OID");
        let parent_shas = text_field(record[1], "commit parents")?
            .split_whitespace()
            .map(|parent| {
                validate_object_id(parent)?;
                Ok(parent.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        commits.push(CommitInventoryEntry {
            sha,
            parent_shas,
            message_headline: text_field(record[2], "commit headline")?,
            authored_at: text_field(record[3], "commit authored date")?,
            committed_at: text_field(record[4], "commit committed date")?,
        });
    }
    ensure!(
        commits.len() == count,
        "Local commit inventory count changed"
    );
    Ok(CommitInventory {
        full_revision: full_revision.clone(),
        commits,
        availability: InventoryAvailability::Complete,
        notice: None,
    })
}

pub fn select_local_comparison(
    path: &Path,
    full_revision: &Revision,
    inventory: &CommitInventory,
    request: ComparisonRequest,
    lazy: bool,
) -> Result<ComparisonSelection> {
    validate_inventory_revision(inventory, full_revision)?;
    let (comparison, metadata, full_pr) = match &request {
        ComparisonRequest::FullPullRequest => (
            if lazy {
                local_pr_inventory(path, full_revision)?
            } else {
                local_pr_comparison(path, full_revision)?
            },
            ComparisonMetadata::default(),
            true,
        ),
        ComparisonRequest::Commit { sha } => {
            let revision = individual_revision(inventory, sha)?;
            (
                load_local_pair(path, &revision, lazy)?,
                ComparisonMetadata {
                    mode: ComparisonMode::Commit { sha: sha.clone() },
                    requested_mode: None,
                    notice: None,
                },
                false,
            )
        }
        ComparisonRequest::CommitRange {
            first_sha,
            last_sha,
        } => {
            let revision = range_revision(inventory, first_sha, last_sha)?;
            (
                load_local_pair(path, &revision, lazy)?,
                ComparisonMetadata {
                    mode: ComparisonMode::CommitRange,
                    requested_mode: None,
                    notice: None,
                },
                false,
            )
        }
        ComparisonRequest::SinceLastReview { baseline } => match baseline {
            BaselineResolution::Found(baseline) => {
                let requested = ComparisonMode::SinceLastReview {
                    reviewed_head_sha: baseline.reviewed_head_sha.clone(),
                };
                let revision = Revision {
                    base_sha: baseline.reviewed_head_sha.clone(),
                    head_sha: full_revision.head_sha.clone(),
                };
                if local_commit_exists(path, &baseline.reviewed_head_sha)? {
                    (
                        load_local_pair(path, &revision, lazy)?,
                        ComparisonMetadata {
                            mode: requested,
                            requested_mode: None,
                            notice: None,
                        },
                        false,
                    )
                } else {
                    (
                        if lazy {
                            local_pr_inventory(path, full_revision)?
                        } else {
                            local_pr_comparison(path, full_revision)?
                        },
                        ComparisonMetadata {
                            mode: ComparisonMode::FullPullRequest,
                            requested_mode: Some(requested),
                            notice: Some("The previous reviewed commit is unavailable locally; showing the full pull request diff.".into()),
                        },
                        true,
                    )
                }
            }
            BaselineResolution::Unavailable(reason) => (
                if lazy {
                    local_pr_inventory(path, full_revision)?
                } else {
                    local_pr_comparison(path, full_revision)?
                },
                ComparisonMetadata {
                    mode: ComparisonMode::FullPullRequest,
                    requested_mode: None,
                    notice: Some(format!(
                        "{} Showing the full pull request diff.",
                        reason.notice()
                    )),
                },
                true,
            ),
        },
    };
    let load_revision = comparison.revision.clone();
    Ok(ComparisonSelection {
        full_revision: full_revision.clone(),
        request,
        comparison,
        metadata,
        local_file_load: lazy.then_some(LocalFileLoadPlan {
            revision: load_revision,
            full_pr,
        }),
    })
}

pub fn select_github_comparison(
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    full_revision: &Revision,
    inventory: &CommitInventory,
    request: ComparisonRequest,
) -> Result<ComparisonSelection> {
    validate_inventory_revision(inventory, full_revision)?;
    let (comparison, metadata) = match &request {
        ComparisonRequest::FullPullRequest => (
            provider.comparison(repo, number, full_revision)?,
            ComparisonMetadata::default(),
        ),
        ComparisonRequest::Commit { sha } => {
            let revision = individual_revision(inventory, sha)?;
            (
                provider.direct_comparison(repo, &revision)?,
                ComparisonMetadata {
                    mode: ComparisonMode::Commit { sha: sha.clone() },
                    requested_mode: None,
                    notice: None,
                },
            )
        }
        ComparisonRequest::CommitRange {
            first_sha,
            last_sha,
        } => {
            let revision = range_revision(inventory, first_sha, last_sha)?;
            (
                provider.direct_comparison(repo, &revision)?,
                ComparisonMetadata {
                    mode: ComparisonMode::CommitRange,
                    requested_mode: None,
                    notice: None,
                },
            )
        }
        ComparisonRequest::SinceLastReview { baseline } => match baseline {
            BaselineResolution::Found(baseline) => {
                let requested = ComparisonMode::SinceLastReview {
                    reviewed_head_sha: baseline.reviewed_head_sha.clone(),
                };
                let revision = Revision {
                    base_sha: baseline.reviewed_head_sha.clone(),
                    head_sha: full_revision.head_sha.clone(),
                };
                match provider.direct_comparison(repo, &revision) {
                    Ok(comparison) => (
                        comparison,
                        ComparisonMetadata {
                            mode: requested,
                            requested_mode: None,
                            notice: None,
                        },
                    ),
                    Err(error) => (
                        provider.comparison(repo, number, full_revision)?,
                        ComparisonMetadata {
                            mode: ComparisonMode::FullPullRequest,
                            requested_mode: Some(requested),
                            notice: Some(format!(
                                "The exact changes since the previous review are unavailable; showing the full pull request diff. Reason: {error:#}"
                            )),
                        },
                    ),
                }
            }
            BaselineResolution::Unavailable(reason) => (
                provider.comparison(repo, number, full_revision)?,
                ComparisonMetadata {
                    mode: ComparisonMode::FullPullRequest,
                    requested_mode: None,
                    notice: Some(format!(
                        "{} Showing the full pull request diff.",
                        reason.notice()
                    )),
                },
            ),
        },
    };
    Ok(ComparisonSelection {
        full_revision: full_revision.clone(),
        request,
        comparison,
        metadata,
        local_file_load: None,
    })
}

fn validate_inventory_revision(
    inventory: &CommitInventory,
    full_revision: &Revision,
) -> Result<()> {
    ensure!(
        inventory.full_revision == *full_revision,
        "Commit inventory belongs to a different published revision"
    );
    Ok(())
}

fn validate_inventory_complete(inventory: &CommitInventory) -> Result<()> {
    ensure!(
        inventory.availability == InventoryAvailability::Complete,
        "Commit inventory is not complete for the selected published revision"
    );
    Ok(())
}

fn individual_revision(inventory: &CommitInventory, sha: &str) -> Result<Revision> {
    validate_inventory_complete(inventory)?;
    validate_object_id(sha)?;
    let commit = inventory
        .commits
        .iter()
        .find(|commit| commit.sha == sha)
        .context("Selected commit is not in the fixed PR commit inventory")?;
    ensure!(
        commit.parent_shas.len() == 1,
        "Individual root and merge commits are unavailable because no single direct parent comparison is unambiguous"
    );
    Ok(Revision {
        base_sha: commit.parent_shas[0].clone(),
        head_sha: commit.sha.clone(),
    })
}

fn range_revision(
    inventory: &CommitInventory,
    first_sha: &str,
    last_sha: &str,
) -> Result<Revision> {
    validate_inventory_complete(inventory)?;
    validate_object_id(first_sha)?;
    validate_object_id(last_sha)?;
    let first = inventory
        .commits
        .iter()
        .position(|commit| commit.sha == first_sha)
        .context("First selected commit is not in the fixed PR commit inventory")?;
    let last = inventory
        .commits
        .iter()
        .position(|commit| commit.sha == last_sha)
        .context("Last selected commit is not in the fixed PR commit inventory")?;
    ensure!(first <= last, "Commit range endpoints are reversed");
    let selected = &inventory.commits[first..=last];
    ensure!(
        selected.iter().all(|commit| commit.parent_shas.len() == 1),
        "Commit ranges containing root or merge commits are unavailable because direct range semantics are ambiguous"
    );
    ensure!(
        selected
            .windows(2)
            .all(|pair| pair[1].parent_shas[0] == pair[0].sha),
        "Selected commits are not a contiguous first-parent range"
    );
    Ok(Revision {
        base_sha: selected[0].parent_shas[0].clone(),
        head_sha: selected.last().expect("non-empty slice").sha.clone(),
    })
}

fn load_local_pair(path: &Path, revision: &Revision, lazy: bool) -> Result<Comparison> {
    if lazy {
        local_inventory(path, revision)
    } else {
        local_comparison(path, revision)
    }
}

fn validate_local_commit(path: &Path, oid: &str) -> Result<()> {
    validate_object_id(oid)?;
    ensure!(
        git(path, &["cat-file", "-t", oid])? == b"commit\n",
        "Revision object {oid} is not a commit"
    );
    Ok(())
}

fn local_commit_exists(path: &Path, oid: &str) -> Result<bool> {
    validate_object_id(oid)?;
    match git(path, &["cat-file", "-t", oid]) {
        Ok(kind) => {
            ensure!(
                kind == b"commit\n",
                "Previous review object is not a commit"
            );
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

fn text_field(bytes: &[u8], label: &str) -> Result<String> {
    Ok(std::str::from_utf8(bytes)
        .with_context(|| format!("Invalid UTF-8 in local {label}"))?
        .to_owned())
}

fn safe_git(path: &Path) -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .arg("--no-pager")
        .args(["-c", "core.fsmonitor=false"])
        .current_dir(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn git<S: AsRef<OsStr>>(path: &Path, args: &[S]) -> Result<Vec<u8>> {
    let mut command = safe_git(path);
    command.args(args).stdout(Stdio::piped()).process_group(0);
    let mut child = command.spawn().context("Start local commit reader")?;
    let mut stdout = child.stdout.take().context("Missing Git output pipe")?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let reader_exceeded = exceeded.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                let count = stdout.read(&mut chunk)?;
                if count == 0 {
                    return Ok(bytes);
                }
                if bytes.len() + count > MAX_LOCAL_INVENTORY_BYTES {
                    reader_exceeded.store(true, Ordering::Release);
                    return Ok(bytes);
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
        })();
        let _ = sender.send(result);
    });
    let started = Instant::now();
    let mut bytes = None;
    loop {
        if exceeded.load(Ordering::Acquire) || started.elapsed() >= GIT_READ_DEADLINE {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &format!("-{}", child.id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
            bail!("Local commit inventory exceeded its output or time limit");
        }
        if bytes.is_none() {
            match receiver.try_recv() {
                Ok(result) => bytes = Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("Local commit output reader stopped");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(status) = child.try_wait().context("Wait for local commit reader")?
            && let Some(bytes) = bytes
        {
            ensure!(
                !exceeded.load(Ordering::Acquire),
                "Local commit inventory exceeded its output limit"
            );
            ensure!(
                status.success(),
                "Local Git commit read failed (exit {:?})",
                status.code()
            );
            return bytes.context("Read local commit output");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
