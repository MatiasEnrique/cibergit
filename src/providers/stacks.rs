//! Read-only GitHub stack membership and immutable net comparisons.

use super::{GithubProvider, Session, validate_component, validate_sha};
use crate::{
    domain::{Repository, Revision},
    stacks::{
        BoundaryProof, EffectiveBoundary, NativeStackAvailability, NativeStackRead, StackLayer,
        StackLayerState, StackNetPlan, StackNetSelection, StackPullRequestId, StackRef,
        StackRepository, completed_selection, plan_stack_net, unavailable_selection,
        validate_remaining_layer_heads,
    },
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;

const NATIVE_STACK_PAGE_SIZE: usize = 100;
const MAX_NATIVE_STACK_PAGES: usize = 10;
const MAX_REMOTE_PROOF_COMMITS: usize = 1_000;

impl GithubProvider {
    /// Read a stable native GitHub stack snapshot twice. Unsupported preview
    /// schema, partial data, movement, and caps remain explicit availability
    /// states so callers may use disclosed coordinate inference.
    pub fn native_stack(&self, repo: &Repository, number: u64) -> Result<NativeStackRead> {
        self.validate_repo(repo)?;
        ensure!(
            number > 0 && number <= i32::MAX as u64,
            "Pull request number is outside GitHub GraphQL limits"
        );
        let first = self.read_native_stack_once(repo, number)?;
        if matches!(
            first.availability,
            NativeStackAvailability::Unsupported
                | NativeStackAvailability::Partial
                | NativeStackAvailability::Capped
        ) {
            return Ok(first);
        }
        let second = self.read_native_stack_once(repo, number)?;
        if first != second {
            return Ok(NativeStackRead::unavailable(
                NativeStackAvailability::Partial,
                "Native stack membership, ordering, states, or revisions moved during the bounded read; refresh to retry.",
            ));
        }
        Ok(first)
    }

    /// Revalidate the frozen stack, prove one direct pair, and return one
    /// immutable comparison. This never advances an existing PR session.
    pub fn select_stack_net(
        &self,
        repo: &Repository,
        stack: &crate::stacks::StackResolution,
        selected_tip: &StackPullRequestId,
    ) -> Result<StackNetSelection> {
        self.validate_repo(repo)?;
        ensure!(
            stack.repository_key == repo.cache_key(),
            "Stack snapshot belongs to a different account or repository"
        );
        let plan = plan_stack_net(stack, selected_tip)?;
        if let Err(error) = self.revalidate_stack_plan(repo, &plan) {
            return Ok(unavailable_selection(
                plan,
                vec![format!("Frozen stack changed before comparison: {error}")],
            ));
        }
        if plan.all_merged {
            return Ok(StackNetSelection {
                selected_tip: plan.selected_tip,
                frozen_layers: plan.frozen_layers,
                frozen_edges: plan.frozen_edges,
                effective_boundary: EffectiveBoundary::AllMerged,
                comparison: None,
            });
        }
        let tip = plan
            .frozen_layers
            .last()
            .context("Selected stack path is empty")?
            .revision
            .head_sha
            .clone();
        let mut reasons = plan.unavailable_reasons.clone();
        for candidate in plan.candidates.clone() {
            if let Some(merged_commit) = &candidate.required_merged_ancestor
                && let Err(error) =
                    self.prove_remote_pair(repo, merged_commit, &candidate.boundary_sha, false)
            {
                reasons.push(format!(
                    "Lowest remaining base {} does not contain provider-revalidated merged result {}: {error}",
                    short_oid(&candidate.boundary_sha),
                    short_oid(merged_commit)
                ));
                continue;
            }
            if let Err(error) = self
                .prove_remote_pair(repo, &candidate.boundary_sha, &tip, true)
                .and_then(|history| validate_remaining_layer_heads(&plan, &history))
            {
                reasons.push(format!(
                    "Boundary {} is unavailable: {error}",
                    short_oid(&candidate.boundary_sha)
                ));
                continue;
            }
            let revision = Revision {
                base_sha: candidate.boundary_sha.clone(),
                head_sha: tip.clone(),
            };
            let comparison = match self.direct_comparison(repo, &revision) {
                Ok(comparison) => comparison,
                Err(error) => {
                    reasons.push(format!(
                        "Boundary {} direct comparison is unavailable: {error}",
                        short_oid(&candidate.boundary_sha)
                    ));
                    continue;
                }
            };
            if let Err(error) = self.revalidate_stack_plan(repo, &plan) {
                return Ok(unavailable_selection(
                    plan,
                    vec![format!("Frozen stack changed during comparison: {error}")],
                ));
            }
            return Ok(completed_selection(
                plan,
                candidate.boundary_sha.clone(),
                candidate.source,
                BoundaryProof::GithubDirectComparison,
                comparison,
            ));
        }
        Ok(unavailable_selection(plan, reasons))
    }

    fn read_native_stack_once(&self, repo: &Repository, number: u64) -> Result<NativeStackRead> {
        let mut session = Session::new(self);
        let mut after = None;
        let mut layers = Vec::new();
        let mut seen_entries = HashSet::new();
        let mut identity: Option<NativeIdentity> = None;
        for page in 1..=MAX_NATIVE_STACK_PAGES {
            let response = match session.graphql::<NativeStackData>(
                NATIVE_STACK_QUERY,
                json!({
                    "owner": repo.owner,
                    "name": repo.name,
                    "number": number,
                    "after": after,
                }),
            ) {
                Ok(response) => response,
                Err(error) => {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Unsupported,
                        format!(
                            "GitHub native stack preview could not be read; no native absence is inferred: {error}"
                        ),
                    ));
                }
            };
            if response.partial {
                return Ok(NativeStackRead::unavailable(
                    NativeStackAvailability::Partial,
                    "GitHub returned partial native stack GraphQL data; no partial membership is installed.",
                ));
            }
            let repository = response
                .data
                .repository
                .context("GitHub omitted the selected repository")?;
            ensure!(
                repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&repo.full_name()),
                "GitHub returned a different selected repository"
            );
            let pull = repository
                .pull_request
                .context("GitHub omitted the selected pull request")?;
            ensure!(
                pull.number == number,
                "GitHub returned a different pull request"
            );
            match (&pull.stack, &pull.stack_entry) {
                (None, None) if page == 1 => return Ok(NativeStackRead::not_member()),
                (Some(_), Some(_)) => {}
                _ => {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "GitHub returned inconsistent native stack and stack-entry fields.",
                    ));
                }
            }
            let stack = pull.stack.expect("checked");
            let selected_entry = pull.stack_entry.expect("checked");
            ensure!(
                selected_entry.stack.id == stack.id,
                "Selected stack entry points to a different stack"
            );
            let page_identity = NativeIdentity {
                stack_node_id: stack.id.clone(),
                stack_number: positive_u64(stack.number, "native stack number")?,
                stack_base_ref: stack.base_ref_name.clone(),
                stack_size: positive_usize(stack.size, "native stack size")?,
                total_count: nonnegative_usize(
                    stack.entries.total_count,
                    "native stack entry count",
                )?,
                selected_entry_id: selected_entry.id.clone(),
                selected_position: positive_u32(
                    selected_entry.position,
                    "selected native stack position",
                )?,
            };
            validate_native_identity(&page_identity)?;
            if page_identity.stack_size > crate::stacks::MAX_STACK_LAYERS
                || page_identity.total_count > crate::stacks::MAX_STACK_LAYERS
            {
                return Ok(NativeStackRead::unavailable(
                    NativeStackAvailability::Capped,
                    format!(
                        "Native stack exceeds the {}-layer bound.",
                        crate::stacks::MAX_STACK_LAYERS
                    ),
                ));
            }
            if page_identity.stack_size != page_identity.total_count {
                return Ok(NativeStackRead::unavailable(
                    NativeStackAvailability::Partial,
                    "Native stack size disagrees with its entry connection; no partial membership is installed.",
                ));
            }
            if let Some(expected) = &identity {
                if expected != &page_identity {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "Native stack identity changed during pagination; no partial membership is installed.",
                    ));
                }
            } else {
                identity = Some(page_identity);
            }
            ensure!(
                stack.entries.nodes.len() <= NATIVE_STACK_PAGE_SIZE,
                "Invalid native stack page size"
            );
            for entry in stack.entries.nodes {
                let Some(entry) = entry else {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "Native stack page omitted an entry; no partial membership is installed.",
                    ));
                };
                if entry.pull_request.is_none() {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "Native stack entry omitted its pull request; no partial membership is installed.",
                    ));
                }
                ensure!(
                    valid_node_id(&entry.id) && seen_entries.insert(entry.id.clone()),
                    "Invalid or repeated native stack entry"
                );
                layers.push(native_layer(repo, entry)?);
            }
            if !stack.entries.page_info.has_next_page {
                let identity = identity.context("Native stack omitted its identity")?;
                if layers.len() != identity.total_count {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "Native stack ended before its reported total; no partial membership is installed.",
                    ));
                }
                layers.sort_by_key(|layer| layer.native_position);
                if !layers
                    .iter()
                    .enumerate()
                    .all(|(index, layer)| layer.native_position == u32::try_from(index + 1).ok())
                {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "Native stack positions are incomplete; no partial membership is installed.",
                    ));
                }
                let selected = layers
                    .iter()
                    .find(|layer| layer.id.number == number)
                    .filter(|selected| {
                        selected.native_entry_id.as_deref()
                            == Some(identity.selected_entry_id.as_str())
                            && selected.native_position == Some(identity.selected_position)
                    });
                if selected.is_none() {
                    return Ok(NativeStackRead::unavailable(
                        NativeStackAvailability::Partial,
                        "Selected pull request entry changed or disappeared during native pagination.",
                    ));
                }
                return Ok(NativeStackRead {
                    availability: NativeStackAvailability::Complete,
                    stack_node_id: Some(identity.stack_node_id),
                    stack_number: Some(identity.stack_number),
                    stack_base_ref: Some(identity.stack_base_ref),
                    selected_entry_id: Some(identity.selected_entry_id),
                    layers,
                    notice: None,
                });
            }
            after = Some(
                stack
                    .entries
                    .page_info
                    .end_cursor
                    .filter(|cursor| valid_cursor(cursor))
                    .context("Native stack page omitted its continuation cursor")?,
            );
            if page == MAX_NATIVE_STACK_PAGES {
                return Ok(NativeStackRead::unavailable(
                    NativeStackAvailability::Capped,
                    format!("Native stack reached the {MAX_NATIVE_STACK_PAGES}-page read limit."),
                ));
            }
        }
        unreachable!("bounded native stack loop returns")
    }

    fn revalidate_stack_plan(&self, repo: &Repository, plan: &StackNetPlan) -> Result<()> {
        let mut session = Session::new(self);
        for frozen in &plan.frozen_layers {
            let current: FrozenApiPull = session.get(&format!(
                "repos/{}/pulls/{}",
                repo.full_name(),
                frozen.id.number
            ))?;
            let current = current.into_layer(repo, frozen)?;
            ensure!(
                current == *frozen,
                "Pull request {} state, revision, refs, repositories, or merge result changed",
                frozen.id.number
            );
        }
        if let Some(expected) = &plan.frozen_native {
            let selected_number = expected
                .layers
                .iter()
                .find(|layer| layer.native_entry_id == expected.selected_entry_id)
                .map(|layer| layer.id.number)
                .context("Frozen native snapshot omitted its selected entry")?;
            let current = self.native_stack(repo, selected_number)?;
            ensure!(
                current == *expected,
                "Native stack identity, membership, ordering, states, or revisions changed"
            );
        }
        Ok(())
    }

    fn prove_remote_pair(
        &self,
        repo: &Repository,
        ancestor: &str,
        descendant: &str,
        require_linear: bool,
    ) -> Result<Vec<String>> {
        validate_sha(ancestor)?;
        validate_sha(descendant)?;
        if ancestor == descendant {
            return Ok(vec![ancestor.to_owned()]);
        }
        let mut session = Session::new(self);
        let endpoint = |page| {
            format!(
                "repos/{}/compare/{ancestor}...{descendant}?per_page={NATIVE_STACK_PAGE_SIZE}&page={page}",
                repo.full_name()
            )
        };
        let first: ProofComparison = session.get(&endpoint(1))?;
        validate_proof_page(&first, ancestor, descendant)?;
        ensure!(
            first.total_commits > 0 && first.total_commits <= MAX_REMOTE_PROOF_COMMITS,
            "Remote ancestry proof is empty or exceeds the bounded commit limit"
        );
        let pages = first.total_commits.div_ceil(NATIVE_STACK_PAGE_SIZE);
        ensure!(
            pages <= MAX_NATIVE_STACK_PAGES,
            "Remote ancestry proof exceeds the bounded page limit"
        );
        let mut commits = first.commits;
        ensure!(
            commits.len() == first.total_commits.min(NATIVE_STACK_PAGE_SIZE),
            "Remote ancestry proof first page is truncated"
        );
        for page in 2..=pages {
            let next: ProofComparison = session.get(&endpoint(page))?;
            validate_proof_page(&next, ancestor, descendant)?;
            ensure!(
                next.total_commits == first.total_commits
                    && next.commits.len()
                        == if page == pages {
                            (first.total_commits - 1) % NATIVE_STACK_PAGE_SIZE + 1
                        } else {
                            NATIVE_STACK_PAGE_SIZE
                        },
                "Remote ancestry proof changed or truncated during pagination"
            );
            commits.extend(next.commits);
        }
        ensure!(
            commits.len() == first.total_commits
                && commits
                    .last()
                    .is_some_and(|commit| commit.sha == descendant),
            "Remote ancestry proof did not end at the requested descendant"
        );
        let mut seen = HashSet::new();
        for commit in &commits {
            validate_sha(&commit.sha)?;
            ensure!(seen.insert(commit.sha.clone()), "Repeated proof commit");
            for parent in &commit.parents {
                validate_sha(&parent.sha)?;
            }
        }
        if require_linear {
            let mut expected_parent = ancestor;
            for commit in &commits {
                ensure!(
                    commit.parents.len() == 1 && commit.parents[0].sha == expected_parent,
                    "Internal merge or non-linear history makes the direct path unprovable"
                );
                expected_parent = &commit.sha;
            }
        }
        let mut history = Vec::with_capacity(commits.len() + 1);
        history.push(ancestor.to_owned());
        history.extend(commits.into_iter().map(|commit| commit.sha));
        Ok(history)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeIdentity {
    stack_node_id: String,
    stack_number: u64,
    stack_base_ref: String,
    stack_size: usize,
    total_count: usize,
    selected_entry_id: String,
    selected_position: u32,
}

fn validate_native_identity(identity: &NativeIdentity) -> Result<()> {
    ensure!(
        valid_node_id(&identity.stack_node_id)
            && valid_node_id(&identity.selected_entry_id)
            && valid_branch(&identity.stack_base_ref)
            && identity.stack_number > 0
            && identity.stack_size > 0
            && identity.selected_position > 0,
        "Invalid native stack identity"
    );
    Ok(())
}

fn native_layer(repo: &Repository, entry: NativeEntry) -> Result<StackLayer> {
    let position = positive_u32(entry.position, "native stack position")?;
    let pull = entry
        .pull_request
        .context("Native stack entry omitted its pull request")?;
    ensure!(pull.number > 0, "Invalid native stack pull request number");
    let target = stack_repository(&pull.base_repository)?;
    ensure!(
        target.matches_repo(repo),
        "Native stack entry belongs to a different target repository"
    );
    ensure!(
        pull.repository
            .name_with_owner
            .eq_ignore_ascii_case(&repo.full_name()),
        "Native stack entry belongs to a different pull request repository"
    );
    let source = pull
        .head_repository
        .as_ref()
        .map(stack_repository)
        .transpose()?;
    ensure!(
        valid_branch(&pull.base_ref_name) && valid_branch(&pull.head_ref_name),
        "Invalid native stack branch"
    );
    validate_sha(&pull.base_ref_oid)?;
    validate_sha(&pull.head_ref_oid)?;
    let state = layer_state(&pull.state, pull.merged)?;
    let merged_commit_oid = if state == StackLayerState::Merged {
        pull.merge_commit.map(|commit| commit.oid)
    } else {
        None
    };
    if let Some(oid) = &merged_commit_oid {
        validate_sha(oid)?;
    }
    Ok(StackLayer {
        id: StackPullRequestId {
            repository: target.clone(),
            number: pull.number,
        },
        native_entry_id: Some(entry.id),
        native_position: Some(position),
        source: StackRef {
            repository: source,
            branch: pull.head_ref_name,
            oid: pull.head_ref_oid.clone(),
        },
        target: StackRef {
            repository: Some(target),
            branch: pull.base_ref_name,
            oid: pull.base_ref_oid.clone(),
        },
        revision: Revision {
            base_sha: pull.base_ref_oid,
            head_sha: pull.head_ref_oid,
        },
        state,
        merged_commit_oid,
    })
}

fn stack_repository(repository: &NativeRepository) -> Result<StackRepository> {
    validate_component(&repository.owner.login, false)?;
    validate_component(&repository.name, true)?;
    ensure!(
        repository
            .name_with_owner
            .eq_ignore_ascii_case(&format!("{}/{}", repository.owner.login, repository.name)),
        "GitHub repository identity fields disagree"
    );
    Ok(StackRepository {
        host: "github.com".into(),
        owner: repository.owner.login.clone(),
        name: repository.name.clone(),
    })
}

impl StackRepository {
    fn matches_repo(&self, repo: &Repository) -> bool {
        self.host.eq_ignore_ascii_case(&repo.host)
            && self.owner.eq_ignore_ascii_case(&repo.owner)
            && self.name.eq_ignore_ascii_case(&repo.name)
    }
}

fn layer_state(state: &str, merged: bool) -> Result<StackLayerState> {
    match (state, merged) {
        ("MERGED", true) | ("CLOSED", true) => Ok(StackLayerState::Merged),
        ("OPEN", false) => Ok(StackLayerState::Open),
        ("CLOSED", false) => Ok(StackLayerState::ClosedUnmerged),
        _ => anyhow::bail!("Invalid or inconsistent stack pull request state"),
    }
}

#[derive(Deserialize)]
struct FrozenApiPull {
    number: u64,
    state: String,
    merged_at: Option<String>,
    merge_commit_sha: Option<String>,
    html_url: String,
    base: FrozenApiRef,
    head: FrozenApiRef,
}

#[derive(Deserialize)]
struct FrozenApiRef {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<FrozenApiRepository>,
}

#[derive(Deserialize)]
struct FrozenApiRepository {
    name: String,
    owner: FrozenApiOwner,
}

#[derive(Deserialize)]
struct FrozenApiOwner {
    login: String,
}

impl FrozenApiPull {
    fn into_layer(self, repo: &Repository, frozen: &StackLayer) -> Result<StackLayer> {
        ensure!(
            self.number == frozen.id.number
                && self.html_url
                    == format!(
                        "https://{}/{}/pull/{}",
                        repo.host,
                        repo.full_name(),
                        self.number
                    ),
            "GitHub returned a different frozen pull request"
        );
        let target = frozen_repository(self.base.repo.as_ref())?
            .context("Frozen pull request target repository is unavailable")?;
        ensure!(
            target.matches_repo(repo),
            "Frozen pull request target repository changed"
        );
        let source = frozen_repository(self.head.repo.as_ref())?;
        validate_sha(&self.base.sha)?;
        validate_sha(&self.head.sha)?;
        ensure!(
            valid_branch(&self.base.branch) && valid_branch(&self.head.branch),
            "Invalid frozen pull request branch"
        );
        let state = if self.merged_at.is_some() {
            StackLayerState::Merged
        } else if self.state == "open" {
            StackLayerState::Open
        } else if self.state == "closed" {
            StackLayerState::ClosedUnmerged
        } else {
            anyhow::bail!("Invalid frozen pull request state")
        };
        let merged_commit_oid = if state == StackLayerState::Merged {
            self.merge_commit_sha
        } else {
            None
        };
        if let Some(oid) = &merged_commit_oid {
            validate_sha(oid)?;
        }
        Ok(StackLayer {
            id: frozen.id.clone(),
            native_entry_id: frozen.native_entry_id.clone(),
            native_position: frozen.native_position,
            source: StackRef {
                repository: source,
                branch: self.head.branch,
                oid: self.head.sha.clone(),
            },
            target: StackRef {
                repository: Some(target),
                branch: self.base.branch,
                oid: self.base.sha.clone(),
            },
            revision: Revision {
                base_sha: self.base.sha,
                head_sha: self.head.sha,
            },
            state,
            merged_commit_oid,
        })
    }
}

fn frozen_repository(repository: Option<&FrozenApiRepository>) -> Result<Option<StackRepository>> {
    repository
        .map(|repository| {
            validate_component(&repository.owner.login, false)?;
            validate_component(&repository.name, true)?;
            Ok(StackRepository {
                host: "github.com".into(),
                owner: repository.owner.login.clone(),
                name: repository.name.clone(),
            })
        })
        .transpose()
}

#[derive(Deserialize)]
struct ProofComparison {
    base_commit: ProofCommitIdentity,
    merge_base_commit: ProofCommitIdentity,
    total_commits: usize,
    #[serde(default)]
    commits: Vec<ProofCommit>,
}

#[derive(Deserialize)]
struct ProofCommitIdentity {
    sha: String,
}

#[derive(Deserialize)]
struct ProofCommit {
    sha: String,
    #[serde(default)]
    parents: Vec<ProofCommitIdentity>,
}

fn validate_proof_page(
    comparison: &ProofComparison,
    ancestor: &str,
    descendant: &str,
) -> Result<()> {
    ensure!(
        comparison.base_commit.sha == ancestor && comparison.merge_base_commit.sha == ancestor,
        "GitHub three-dot comparison does not prove the requested ancestor"
    );
    validate_sha(&comparison.base_commit.sha)?;
    validate_sha(&comparison.merge_base_commit.sha)?;
    ensure!(
        comparison
            .commits
            .last()
            .is_none_or(|commit| commit.sha != ancestor || ancestor == descendant),
        "Invalid remote ancestry proof page"
    );
    Ok(())
}

fn positive_u64(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .with_context(|| format!("Invalid {label}"))
}

fn positive_u32(value: i64, label: &str) -> Result<u32> {
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .with_context(|| format!("Invalid {label}"))
}

fn positive_usize(value: i64, label: &str) -> Result<usize> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .with_context(|| format!("Invalid {label}"))
}

fn nonnegative_usize(value: i64, label: &str) -> Result<usize> {
    usize::try_from(value).with_context(|| format!("Invalid {label}"))
}

fn valid_node_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1_024
        && !value.contains('\0')
        && !value.chars().any(char::is_whitespace)
}

fn valid_branch(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1_024
        && !value.starts_with('-')
        && !value.contains('\0')
        && !value.chars().any(char::is_control)
}

fn valid_cursor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8_192
        && !value.contains('\0')
        && !value.chars().any(char::is_control)
}

fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

#[derive(Deserialize)]
struct NativeStackData {
    repository: Option<NativeStackRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeStackRepository {
    name_with_owner: String,
    pull_request: Option<NativeSelectedPull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeSelectedPull {
    number: u64,
    stack: Option<NativeStack>,
    stack_entry: Option<NativeSelectedEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeSelectedEntry {
    id: String,
    position: i64,
    stack: NativeSelectedStack,
}

#[derive(Deserialize)]
struct NativeSelectedStack {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeStack {
    id: String,
    number: i64,
    size: i64,
    base_ref_name: String,
    entries: NativeEntryConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeEntryConnection {
    total_count: i64,
    nodes: Vec<Option<NativeEntry>>,
    page_info: NativePageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativePageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeEntry {
    id: String,
    position: i64,
    pull_request: Option<NativeLayerPull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeLayerPull {
    number: u64,
    state: String,
    merged: bool,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    repository: NativeRepository,
    base_repository: NativeRepository,
    head_repository: Option<NativeRepository>,
    merge_commit: Option<NativeCommit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeRepository {
    name_with_owner: String,
    name: String,
    owner: NativeOwner,
}

#[derive(Deserialize)]
struct NativeOwner {
    login: String,
}

#[derive(Deserialize)]
struct NativeCommit {
    oid: String,
}

const NATIVE_STACK_QUERY: &str = r#"query PullRequestStackMembership(
  $owner: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequest(number: $number) {
      number
      stackEntry { id position stack { id } }
      stack {
        id number size baseRefName
        entries(first: 100, after: $after) {
          totalCount
          pageInfo { hasNextPage endCursor }
          nodes {
            id position
            pullRequest {
              number state merged baseRefName baseRefOid headRefName headRefOid
              repository { nameWithOwner name owner { login } }
              baseRepository { nameWithOwner name owner { login } }
              headRepository { nameWithOwner name owner { login } }
              mergeCommit { oid }
            }
          }
        }
      }
    }
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::Account,
        providers::Runner,
        stacks::{EffectiveBoundary, StackEdgeProvenance, StackResolution, resolve_stack},
    };
    use serde_json::{Value, json};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
    use tempfile::{TempDir, tempdir};

    fn account() -> Account {
        Account {
            host: "github.com".into(),
            login: "alice".into(),
        }
    }

    fn repo() -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "owner".into(),
            name: "repo".into(),
            account: account(),
            local_path: None,
        }
    }

    fn oid(character: char) -> String {
        character.to_string().repeat(40)
    }

    fn native_response(first_head: &str, errors: bool) -> Value {
        let mut response = json!({
            "data": {
                "repository": {
                    "nameWithOwner": "owner/repo",
                    "pullRequest": {
                        "number": 1,
                        "stackEntry": {"id": "entry-1", "position": 1, "stack": {"id": "stack-1"}},
                        "stack": {
                            "id": "stack-1",
                            "number": 9,
                            "size": 2,
                            "baseRefName": "main",
                            "entries": {
                                "totalCount": 2,
                                "pageInfo": {"hasNextPage": false, "endCursor": null},
                                "nodes": [
                                    native_entry(1, 1, "entry-1", "lower", "main", &oid('a'), first_head, "OPEN", false, None),
                                    native_entry(2, 2, "entry-2", "upper", "lower", first_head, &oid('c'), "OPEN", false, None)
                                ]
                            }
                        }
                    }
                }
            }
        });
        if errors {
            response["errors"] = json!([{"message": "preview partial"}]);
        }
        response
    }

    #[allow(clippy::too_many_arguments)]
    fn native_entry(
        number: u64,
        position: u32,
        entry: &str,
        head_branch: &str,
        base_branch: &str,
        base_oid: &str,
        head_oid: &str,
        state: &str,
        merged: bool,
        merge_commit: Option<&str>,
    ) -> Value {
        json!({
            "id": entry,
            "position": position,
            "pullRequest": {
                "number": number,
                "state": state,
                "merged": merged,
                "baseRefName": base_branch,
                "baseRefOid": base_oid,
                "headRefName": head_branch,
                "headRefOid": head_oid,
                "repository": repository_json(),
                "baseRepository": repository_json(),
                "headRepository": repository_json(),
                "mergeCommit": merge_commit.map(|oid| json!({"oid": oid}))
            }
        })
    }

    fn repository_json() -> Value {
        json!({
            "nameWithOwner": "owner/repo",
            "name": "repo",
            "owner": {"login": "owner"}
        })
    }

    fn graphql(response: Value) -> Value {
        json!({"kind": "graphql", "response": response})
    }

    fn get(endpoint: &str, response: Value) -> Value {
        json!({"kind": "get", "endpoint": endpoint, "response": response})
    }

    fn fixture(steps: Vec<Value>) -> (TempDir, GithubProvider) {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("steps.json"),
            serde_json::to_vec(&json!({"steps": steps})).unwrap(),
        )
        .unwrap();
        let executable = dir.path().join("gh");
        fs::write(
            &executable,
            r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
config = json.loads((root / 'steps.json').read_text())
args = sys.argv[1:]
if args[:2] == ['auth', 'token']:
    assert args == ['auth', 'token', '--hostname', 'github.com', '--user', 'alice']
    assert 'GH_TOKEN' not in os.environ
    print('fixture-private-token')
    sys.exit(0)
counter = root / 'count'
index = int(counter.read_text()) if counter.exists() else 0
step = config['steps'][index]
assert os.environ.get('GH_TOKEN') == 'fixture-private-token'
if step['kind'] == 'graphql':
    assert args == ['api', '--hostname', 'github.com', '--method', 'POST', '--header', 'Accept: application/vnd.github+json', '--header', 'X-GitHub-Api-Version: 2026-03-10', 'graphql', '--input', '-']
    payload = json.load(sys.stdin)
    assert payload['query'].lstrip().startswith('query ')
    assert 'mutation' not in payload['query']
    assert payload['variables']['owner'] == 'owner'
    assert payload['variables']['name'] == 'repo'
else:
    assert args == ['api', '--hostname', 'github.com', '--method', 'GET', '--header', 'Accept: application/vnd.github+json', '--header', 'X-GitHub-Api-Version: 2026-03-10', step['endpoint']]
counter.write_text(str(index + 1))
print(json.dumps(step['response']))
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = GithubProvider {
            account: account(),
            runner: Runner {
                gh: executable,
                timeout: Duration::from_secs(30),
                ..Runner::default()
            },
        };
        (dir, provider)
    }

    fn exhausted(dir: &Path, expected: usize) {
        assert_eq!(
            fs::read_to_string(dir.join("count"))
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            expected
        );
    }

    #[test]
    fn native_stack_requires_two_identical_complete_reads() {
        let response = native_response(&oid('b'), false);
        let (dir, provider) = fixture(vec![graphql(response.clone()), graphql(response)]);
        let native = provider.native_stack(&repo(), 1).unwrap();
        assert_eq!(native.availability, NativeStackAvailability::Complete);
        assert_eq!(native.layers.len(), 2);
        assert_eq!(native.layers[0].native_position, Some(1));
        assert_eq!(native.layers[1].source.oid, oid('c'));
        exhausted(dir.path(), 2);
    }

    #[test]
    fn native_stack_movement_is_partial_not_a_stale_prefix() {
        let (dir, provider) = fixture(vec![
            graphql(native_response(&oid('b'), false)),
            graphql(native_response(&oid('d'), false)),
        ]);
        let native = provider.native_stack(&repo(), 1).unwrap();
        assert_eq!(native.availability, NativeStackAvailability::Partial);
        assert!(native.layers.is_empty());
        assert!(native.notice.unwrap().contains("moved"));
        exhausted(dir.path(), 2);
    }

    #[test]
    fn preview_unsupported_partial_and_capped_are_distinct() {
        let cases = [
            (
                json!({"errors": [{"message": "Cannot query field stack"}]}),
                NativeStackAvailability::Unsupported,
            ),
            (
                native_response(&oid('b'), true),
                NativeStackAvailability::Partial,
            ),
            (
                json!({
                    "data": {"repository": {
                        "nameWithOwner": "owner/repo",
                        "pullRequest": {
                            "number": 1,
                            "stackEntry": {"id": "entry-1", "position": 1, "stack": {"id": "stack-1"}},
                            "stack": {
                                "id": "stack-1", "number": 1, "size": 1001, "baseRefName": "main",
                                "entries": {"totalCount": 1001, "nodes": [], "pageInfo": {"hasNextPage": true, "endCursor": "next"}}
                            }
                        }
                    }}
                }),
                NativeStackAvailability::Capped,
            ),
        ];
        for (response, expected) in cases {
            let (dir, provider) = fixture(vec![graphql(response)]);
            let native = provider.native_stack(&repo(), 1).unwrap();
            assert_eq!(native.availability, expected);
            assert!(native.layers.is_empty());
            exhausted(dir.path(), 1);
        }
    }

    fn layer(
        number: u64,
        source_branch: &str,
        target_branch: &str,
        base: &str,
        head: &str,
        state: StackLayerState,
        merge_commit: Option<&str>,
    ) -> StackLayer {
        let repository = StackRepository::from_repository(&repo());
        StackLayer {
            id: StackPullRequestId {
                repository: repository.clone(),
                number,
            },
            native_entry_id: None,
            native_position: None,
            source: StackRef {
                repository: Some(repository.clone()),
                branch: source_branch.into(),
                oid: head.into(),
            },
            target: StackRef {
                repository: Some(repository.clone()),
                branch: target_branch.into(),
                oid: base.into(),
            },
            revision: Revision {
                base_sha: base.into(),
                head_sha: head.into(),
            },
            state,
            merged_commit_oid: merge_commit.map(str::to_owned),
        }
    }

    fn rest_pull(layer: &StackLayer, merge_commit: Option<&str>) -> Value {
        let state = if layer.state == StackLayerState::Open {
            "open"
        } else {
            "closed"
        };
        json!({
            "number": layer.id.number,
            "state": state,
            "merged_at": (layer.state == StackLayerState::Merged).then_some("2026-09-13T00:00:00Z"),
            "merge_commit_sha": merge_commit,
            "html_url": format!("https://github.com/owner/repo/pull/{}", layer.id.number),
            "base": {"sha": layer.target.oid, "ref": layer.target.branch, "repo": {"name": "repo", "owner": {"login": "owner"}}},
            "head": {"sha": layer.source.oid, "ref": layer.source.branch, "repo": {"name": "repo", "owner": {"login": "owner"}}}
        })
    }

    #[test]
    fn execution_revalidates_merge_commit_for_the_exact_frozen_pr() {
        let base = oid('a');
        let lower_head = oid('b');
        let merged_commit = oid('c');
        let tip = oid('d');
        let layers = vec![
            layer(
                1,
                "lower",
                "main",
                &base,
                &lower_head,
                StackLayerState::Merged,
                Some(&merged_commit),
            ),
            layer(
                2,
                "upper",
                "lower",
                &lower_head,
                &tip,
                StackLayerState::Open,
                None,
            ),
        ];
        let resolution: StackResolution = resolve_stack(
            &repo(),
            &layers[1].id,
            &layers,
            &NativeStackRead::not_member(),
            None,
        )
        .unwrap();
        assert!(
            resolution
                .edges
                .iter()
                .all(|edge| edge.provenance == StackEdgeProvenance::Inferred)
        );
        let (dir, provider) = fixture(vec![get(
            "repos/owner/repo/pulls/1",
            rest_pull(&layers[0], Some(&oid('e'))),
        )]);
        let selection = provider
            .select_stack_net(&repo(), &resolution, &layers[1].id)
            .unwrap();
        assert!(matches!(
            selection.effective_boundary,
            EffectiveBoundary::Unavailable { ref reason }
                if reason.contains("merge result changed")
        ));
        assert!(selection.comparison.is_none());
        exhausted(dir.path(), 1);
    }

    #[test]
    fn remote_net_refuses_a_frozen_lower_head_missing_from_the_tip_history() {
        let base = oid('a');
        let old_lower = oid('b');
        let advanced_lower = oid('d');
        let tip = oid('c');
        let layers = vec![
            layer(
                1,
                "lower",
                "main",
                &base,
                &advanced_lower,
                StackLayerState::Open,
                None,
            ),
            layer(
                2,
                "upper",
                "lower",
                &advanced_lower,
                &tip,
                StackLayerState::Open,
                None,
            ),
        ];
        let resolution = resolve_stack(
            &repo(),
            &layers[1].id,
            &layers,
            &NativeStackRead::not_member(),
            None,
        )
        .unwrap();
        let (dir, provider) = fixture(vec![
            get("repos/owner/repo/pulls/1", rest_pull(&layers[0], None)),
            get("repos/owner/repo/pulls/2", rest_pull(&layers[1], None)),
            get(
                &format!("repos/owner/repo/compare/{base}...{tip}?per_page=100&page=1"),
                json!({
                    "base_commit": {"sha": base}, "merge_base_commit": {"sha": base},
                    "total_commits": 2, "commits": [
                        {"sha": old_lower, "parents": [{"sha": base}]},
                        {"sha": tip, "parents": [{"sha": old_lower}]}
                    ]
                }),
            ),
        ]);
        let selection = provider
            .select_stack_net(&repo(), &resolution, &layers[1].id)
            .unwrap();
        assert!(
            matches!(selection.effective_boundary, EffectiveBoundary::Unavailable { ref reason }
            if reason.contains("head is absent"))
        );
        assert!(selection.comparison.is_none());
        // Refusal happens before loading a direct file comparison.
        exhausted(dir.path(), 3);
    }
}
