//! Provider-independent stack relationships and net remaining comparisons.
//!
//! Stack discovery is immutable input to this module. The selected tip is always
//! explicit, personal corrections are caller-owned serialized data, and a net
//! comparison is one direct effective-boundary-to-tip tree pair.

use crate::{
    comparisons::{InventoryAvailability, local_commit_inventory},
    domain::{Account, Comparison, Repository, Revision},
    review::{local_comparison, validate_object_id},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

pub const MAX_STACK_LAYERS: usize = 1_000;
pub const PERSONAL_CORRECTIONS_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StackRepository {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl StackRepository {
    pub fn from_repository(repository: &Repository) -> Self {
        Self {
            host: repository.host.clone(),
            owner: repository.owner.clone(),
            name: repository.name.clone(),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StackPullRequestId {
    pub repository: StackRepository,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRef {
    /// `None` is preserved for a deleted or unavailable source repository.
    pub repository: Option<StackRepository>,
    pub branch: String,
    pub oid: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackLayerState {
    Open,
    Merged,
    ClosedUnmerged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLayer {
    pub id: StackPullRequestId,
    pub native_entry_id: Option<String>,
    pub native_position: Option<u32>,
    pub source: StackRef,
    pub target: StackRef,
    pub revision: Revision,
    pub state: StackLayerState,
    /// Provider-observed commit created by merging this exact PR. This may be
    /// absent for inaccessible or historical merges; absence fails closed for
    /// a post-merge-base fallback.
    pub merged_commit_oid: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackEdgeProvenance {
    Native,
    Inferred,
    Personal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackEdge {
    pub parent: StackPullRequestId,
    pub child: StackPullRequestId,
    pub provenance: StackEdgeProvenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeStackAvailability {
    Complete,
    NotMember,
    Unsupported,
    Partial,
    Capped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeStackRead {
    pub availability: NativeStackAvailability,
    pub stack_node_id: Option<String>,
    pub stack_number: Option<u64>,
    pub stack_base_ref: Option<String>,
    pub selected_entry_id: Option<String>,
    pub layers: Vec<StackLayer>,
    pub notice: Option<String>,
}

impl NativeStackRead {
    pub fn not_member() -> Self {
        Self {
            availability: NativeStackAvailability::NotMember,
            stack_node_id: None,
            stack_number: None,
            stack_base_ref: None,
            selected_entry_id: None,
            layers: Vec::new(),
            notice: None,
        }
    }

    pub fn unavailable(availability: NativeStackAvailability, notice: impl Into<String>) -> Self {
        debug_assert!(availability != NativeStackAvailability::Complete);
        Self {
            availability,
            stack_node_id: None,
            stack_number: None,
            stack_base_ref: None,
            selected_entry_id: None,
            layers: Vec::new(),
            notice: Some(notice.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonalStackCorrections {
    pub schema_version: u32,
    pub repository_key: String,
    pub account: Account,
    pub edges: Vec<StackEdge>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackResolution {
    pub repository_key: String,
    pub selected: StackPullRequestId,
    pub layers: Vec<StackLayer>,
    pub edges: Vec<StackEdge>,
    /// Complete provider snapshot retained so execution can revalidate native
    /// stack identity, membership, ordering, states, and revisions.
    pub native: Option<NativeStackRead>,
    pub complete: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectiveBoundarySource {
    RootTarget { layer: StackPullRequestId },
    MergedLayerHead { layer: StackPullRequestId },
    LowestRemainingTarget { layer: StackPullRequestId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryProof {
    LocalDirectAncestor,
    GithubDirectComparison,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectiveBoundary {
    Proven {
        sha: String,
        source: EffectiveBoundarySource,
        proof: BoundaryProof,
    },
    AllMerged,
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackNetSelection {
    pub selected_tip: StackPullRequestId,
    pub frozen_layers: Vec<StackLayer>,
    pub frozen_edges: Vec<StackEdge>,
    pub effective_boundary: EffectiveBoundary,
    pub comparison: Option<Comparison>,
}

/// Resolve native relationships when a complete native snapshot exists, or
/// infer same-repository source/target relationships with visible provenance.
/// Explicit personal edges replace only the named child relationship.
pub fn resolve_stack(
    repo: &Repository,
    selected: &StackPullRequestId,
    candidates: &[StackLayer],
    native: &NativeStackRead,
    corrections: Option<&PersonalStackCorrections>,
) -> Result<StackResolution> {
    ensure!(selected.number > 0, "Invalid selected pull request number");
    let repository = StackRepository::from_repository(repo);
    ensure!(
        selected.repository.matches(&repository),
        "Selected pull request belongs to a different repository"
    );

    let (layers, mut edges, mut notice) = if native.availability
        == NativeStackAvailability::Complete
    {
        validate_native(native, selected, &repository)?;
        let mut layers = native.layers.clone();
        layers.sort_by_key(|layer| layer.native_position);
        let edges = layers
            .windows(2)
            .map(|pair| StackEdge {
                parent: pair[0].id.clone(),
                child: pair[1].id.clone(),
                provenance: StackEdgeProvenance::Native,
            })
            .collect();
        (layers, edges, native.notice.clone())
    } else {
        validate_layers(candidates, &repository, true)?;
        let edges = infer_edges(candidates)?;
        let notice = match native.availability {
            NativeStackAvailability::NotMember => {
                Some("GitHub reports no native stack membership; relationships are inferred from exact repository and branch coordinates.".into())
            }
            availability => Some(format!(
                "Native stack membership is {availability:?}; relationships are inferred from exact repository and branch coordinates.{}",
                native
                    .notice
                    .as_deref()
                    .map(|value| format!(" {value}"))
                    .unwrap_or_default()
            )),
        };
        (candidates.to_vec(), edges, notice)
    };

    let ids: HashSet<_> = layers.iter().map(|layer| layer.id.clone()).collect();
    ensure!(
        ids.contains(selected),
        "Selected pull request is not in the resolved stack"
    );

    if let Some(corrections) = corrections {
        validate_corrections(repo, corrections, &ids)?;
        for correction in &corrections.edges {
            edges.retain(|edge| edge.child != correction.child);
            edges.push(StackEdge {
                parent: correction.parent.clone(),
                child: correction.child.clone(),
                provenance: StackEdgeProvenance::Personal,
            });
        }
        notice = Some(match notice {
            Some(existing) => format!(
                "{existing} Explicit personal corrections are applied locally and do not change remote pull request targets."
            ),
            None => "Explicit personal corrections are applied locally and do not change remote pull request targets.".into(),
        });
    }

    validate_edges(&edges, &ids)?;
    let resolution = StackResolution {
        repository_key: repo.cache_key(),
        selected: selected.clone(),
        layers,
        edges,
        native: (native.availability == NativeStackAvailability::Complete).then(|| native.clone()),
        complete: true,
        notice,
    };
    selected_path(&resolution, selected)?;
    Ok(resolution)
}

fn validate_native(
    native: &NativeStackRead,
    selected: &StackPullRequestId,
    repository: &StackRepository,
) -> Result<()> {
    ensure!(
        native
            .stack_node_id
            .as_deref()
            .is_some_and(valid_text_identity)
            && native.stack_number.is_some_and(|number| number > 0)
            && native.stack_base_ref.as_deref().is_some_and(valid_branch)
            && native
                .selected_entry_id
                .as_deref()
                .is_some_and(valid_text_identity),
        "Complete native stack omitted its immutable identity"
    );
    validate_layers(&native.layers, repository, false)?;
    ensure!(!native.layers.is_empty(), "Complete native stack is empty");
    let mut positions = HashSet::new();
    let mut selected_entry = None;
    for layer in &native.layers {
        let position = layer
            .native_position
            .context("Complete native stack layer omitted its position")?;
        ensure!(
            position > 0 && positions.insert(position),
            "Invalid or repeated native stack position"
        );
        ensure!(
            layer
                .native_entry_id
                .as_deref()
                .is_some_and(valid_text_identity),
            "Complete native stack layer omitted its entry identity"
        );
        if layer.id == *selected {
            selected_entry = layer.native_entry_id.as_deref();
        }
    }
    ensure!(
        positions.len() == native.layers.len()
            && (1..=native.layers.len() as u32).all(|position| positions.contains(&position)),
        "Native stack positions are not complete and contiguous"
    );
    ensure!(
        selected_entry == native.selected_entry_id.as_deref(),
        "Selected native stack entry identity changed"
    );
    Ok(())
}

fn validate_layers(
    layers: &[StackLayer],
    repository: &StackRepository,
    inference: bool,
) -> Result<()> {
    ensure!(
        !layers.is_empty() && layers.len() <= MAX_STACK_LAYERS,
        "Stack layer count is empty or exceeds the bounded limit"
    );
    let mut ids = HashSet::new();
    for layer in layers {
        ensure!(
            layer.id.number > 0
                && layer.id.repository.matches(repository)
                && ids.insert(layer.id.clone()),
            "Invalid, foreign, or repeated stack pull request"
        );
        let target_repository = layer
            .target
            .repository
            .as_ref()
            .context("Stack layer target repository is unavailable")?;
        ensure!(
            target_repository.matches(repository) && layer.id.repository.matches(target_repository),
            "Stack layer target repository does not match its pull request identity"
        );
        ensure!(
            valid_branch(&layer.source.branch) && valid_branch(&layer.target.branch),
            "Invalid stack branch name"
        );
        validate_object_id(&layer.source.oid)?;
        validate_object_id(&layer.target.oid)?;
        ensure!(
            layer.revision.base_sha == layer.target.oid
                && layer.revision.head_sha == layer.source.oid,
            "Stack layer refs do not match its frozen revision"
        );
        match layer.state {
            StackLayerState::Merged => {
                if let Some(oid) = &layer.merged_commit_oid {
                    validate_object_id(oid)?;
                }
            }
            StackLayerState::Open | StackLayerState::ClosedUnmerged => ensure!(
                layer.merged_commit_oid.is_none(),
                "Only a merged stack layer may carry a merge commit"
            ),
        }
        if inference {
            ensure!(
                layer.source.repository.is_some(),
                "Cannot infer a complete stack with an unavailable source repository"
            );
        }
    }
    Ok(())
}

fn infer_edges(layers: &[StackLayer]) -> Result<Vec<StackEdge>> {
    let mut edges = Vec::new();
    for child in layers {
        let child_target = child.target.repository.as_ref().expect("validated");
        let mut parents = Vec::new();
        for parent in layers.iter().filter(|parent| parent.id != child.id) {
            let parent_source = parent.source.repository.as_ref().expect("validated");
            if child_target.matches(parent_source) && child.target.branch == parent.source.branch {
                parents.push(parent);
            }
        }
        ensure!(
            !child.source.repository.as_ref().is_some_and(|source| {
                source.matches(child_target) && child.source.branch == child.target.branch
            }),
            "Stack inference found a self-cycle"
        );
        ensure!(
            parents.len() <= 1,
            "Stack inference found multiple candidate parents for pull request {}",
            child.id.number
        );
        if let Some(parent) = parents.pop() {
            edges.push(StackEdge {
                parent: parent.id.clone(),
                child: child.id.clone(),
                provenance: StackEdgeProvenance::Inferred,
            });
        }
    }
    Ok(edges)
}

fn validate_corrections(
    repo: &Repository,
    corrections: &PersonalStackCorrections,
    ids: &HashSet<StackPullRequestId>,
) -> Result<()> {
    ensure!(
        corrections.schema_version == PERSONAL_CORRECTIONS_SCHEMA_VERSION,
        "Unsupported personal stack correction schema version"
    );
    ensure!(
        corrections.repository_key == repo.cache_key() && corrections.account == repo.account,
        "Personal stack corrections belong to a different account or repository"
    );
    ensure!(
        corrections.edges.len() <= MAX_STACK_LAYERS,
        "Personal stack corrections exceed the bounded limit"
    );
    let mut children = HashSet::new();
    for edge in &corrections.edges {
        ensure!(
            edge.provenance == StackEdgeProvenance::Personal,
            "Correction edge must be explicitly labelled Personal"
        );
        ensure!(
            edge.parent != edge.child
                && ids.contains(&edge.parent)
                && ids.contains(&edge.child)
                && children.insert(edge.child.clone()),
            "Invalid, foreign, ambiguous, or repeated personal correction"
        );
    }
    Ok(())
}

fn validate_edges(edges: &[StackEdge], ids: &HashSet<StackPullRequestId>) -> Result<()> {
    ensure!(
        edges.len() < ids.len().max(1),
        "Stack relationships cannot form a complete cycle"
    );
    let mut parents = HashSet::new();
    for edge in edges {
        ensure!(
            edge.parent != edge.child
                && ids.contains(&edge.parent)
                && ids.contains(&edge.child)
                && parents.insert(edge.child.clone()),
            "Invalid, foreign, or duplicate stack parent"
        );
    }
    for start in ids {
        let mut seen = HashSet::new();
        let mut current = start;
        while let Some(edge) = edges.iter().find(|edge| edge.child == *current) {
            ensure!(
                seen.insert(current.clone()),
                "Stack relationships contain a cycle"
            );
            current = &edge.parent;
        }
    }
    Ok(())
}

fn selected_path(
    stack: &StackResolution,
    selected_tip: &StackPullRequestId,
) -> Result<(Vec<StackLayer>, Vec<StackEdge>)> {
    ensure!(stack.complete, "Stack relationship snapshot is incomplete");
    ensure!(
        stack.repository_key.is_empty() || stack.repository_key == stack.repository_key.trim(),
        "Invalid stack repository partition"
    );
    let by_id: HashMap<_, _> = stack
        .layers
        .iter()
        .map(|layer| (&layer.id, layer))
        .collect();
    ensure!(
        by_id.len() == stack.layers.len() && by_id.contains_key(selected_tip),
        "Explicit selected tip is absent from the frozen stack"
    );
    let mut by_child = HashMap::new();
    for edge in &stack.edges {
        ensure!(
            by_child.insert(&edge.child, edge).is_none(),
            "Selected stack path has multiple parents"
        );
    }
    let mut current = selected_tip;
    let mut seen = HashSet::new();
    let mut layers = Vec::new();
    let mut edges = Vec::new();
    loop {
        ensure!(
            seen.insert(current.clone()),
            "Selected stack path contains a cycle"
        );
        layers.push(
            (*by_id
                .get(current)
                .context("Selected stack path references an unknown layer")?)
            .clone(),
        );
        let Some(edge) = by_child.get(current) else {
            break;
        };
        edges.push((*edge).clone());
        current = &edge.parent;
    }
    layers.reverse();
    edges.reverse();
    Ok((layers, edges))
}

#[derive(Clone, Debug)]
pub(crate) struct StackNetPlan {
    pub selected_tip: StackPullRequestId,
    pub frozen_layers: Vec<StackLayer>,
    pub frozen_edges: Vec<StackEdge>,
    pub frozen_native: Option<NativeStackRead>,
    pub candidates: Vec<StackBoundaryCandidate>,
    pub unavailable_reasons: Vec<String>,
    pub all_merged: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct StackBoundaryCandidate {
    pub boundary_sha: String,
    pub source: EffectiveBoundarySource,
    /// A fallback base is valid only when the provider-reported merge result
    /// for the immediately lower layer is contained in that frozen base.
    pub required_merged_ancestor: Option<String>,
}

pub(crate) fn plan_stack_net(
    stack: &StackResolution,
    selected_tip: &StackPullRequestId,
) -> Result<StackNetPlan> {
    let (layers, edges) = selected_path(stack, selected_tip)?;
    let selected = layers.last().context("Selected stack path is empty")?;
    ensure!(
        selected.id == *selected_tip,
        "Selected stack path ended at a different pull request"
    );
    let lowest_remaining = layers
        .iter()
        .position(|layer| layer.state != StackLayerState::Merged);
    if lowest_remaining.is_none() {
        return Ok(StackNetPlan {
            selected_tip: selected_tip.clone(),
            frozen_layers: layers,
            frozen_edges: edges,
            frozen_native: stack.native.clone(),
            candidates: Vec::new(),
            unavailable_reasons: Vec::new(),
            all_merged: true,
        });
    }
    let lowest_remaining = lowest_remaining.expect("checked");
    if layers[lowest_remaining..]
        .iter()
        .any(|layer| layer.state == StackLayerState::Merged)
    {
        return Ok(StackNetPlan {
            selected_tip: selected_tip.clone(),
            frozen_layers: layers,
            frozen_edges: edges,
            frozen_native: stack.native.clone(),
            candidates: Vec::new(),
            unavailable_reasons: vec![
                "A merged descendant follows an unmerged dependency; one boundary cannot prove precise remaining-only semantics."
                    .into(),
            ],
            all_merged: false,
        });
    }
    let mut candidates = Vec::new();
    let mut unavailable_reasons = Vec::new();
    if lowest_remaining == 0 {
        candidates.push(StackBoundaryCandidate {
            boundary_sha: layers[0].target.oid.clone(),
            source: EffectiveBoundarySource::RootTarget {
                layer: layers[0].id.clone(),
            },
            required_merged_ancestor: None,
        });
    } else {
        candidates.push(StackBoundaryCandidate {
            boundary_sha: layers[lowest_remaining - 1].source.oid.clone(),
            source: EffectiveBoundarySource::MergedLayerHead {
                layer: layers[lowest_remaining - 1].id.clone(),
            },
            required_merged_ancestor: None,
        });
        if layers[lowest_remaining].target.oid != layers[lowest_remaining - 1].source.oid {
            if let Some(merged_commit_oid) = layers[lowest_remaining - 1].merged_commit_oid.clone()
            {
                candidates.push(StackBoundaryCandidate {
                    boundary_sha: layers[lowest_remaining].target.oid.clone(),
                    source: EffectiveBoundarySource::LowestRemainingTarget {
                        layer: layers[lowest_remaining].id.clone(),
                    },
                    required_merged_ancestor: Some(merged_commit_oid),
                });
            } else {
                unavailable_reasons.push(
                    "The merged lower layer has no provider-observed merge commit, so the lowest remaining base cannot prove exclusion of its changes."
                        .into(),
                );
            }
        }
    }
    Ok(StackNetPlan {
        selected_tip: selected_tip.clone(),
        frozen_layers: layers,
        frozen_edges: edges,
        frozen_native: stack.native.clone(),
        candidates,
        unavailable_reasons,
        all_merged: false,
    })
}

pub(crate) fn unavailable_selection(plan: StackNetPlan, reasons: Vec<String>) -> StackNetSelection {
    StackNetSelection {
        selected_tip: plan.selected_tip,
        frozen_layers: plan.frozen_layers,
        frozen_edges: plan.frozen_edges,
        effective_boundary: EffectiveBoundary::Unavailable {
            reason: reasons.join(" "),
        },
        comparison: None,
    }
}

/// Every remaining layer must be represented by its frozen head in the same
/// ordered history that will be displayed. Branch relationships alone do not
/// prove this after a lower branch advances or is rewritten.
pub(crate) fn validate_remaining_layer_heads(
    plan: &StackNetPlan,
    ordered_history: &[String],
) -> Result<()> {
    let positions: HashMap<_, _> = ordered_history
        .iter()
        .enumerate()
        .map(|(position, oid)| (oid.as_str(), position))
        .collect();
    let mut previous = 0;
    for layer in plan
        .frozen_layers
        .iter()
        .filter(|layer| layer.state != StackLayerState::Merged)
    {
        let position = *positions.get(layer.revision.head_sha.as_str()).with_context(|| {
            format!("Frozen unmerged pull request {} head is absent from the selected tip history; refresh or update the dependent branch before reviewing the stack", layer.id.number)
        })?;
        ensure!(
            position >= previous,
            "Frozen unmerged pull request {} head precedes its dependency in the selected tip history",
            layer.id.number
        );
        previous = position;
    }
    Ok(())
}

pub(crate) fn completed_selection(
    plan: StackNetPlan,
    boundary_sha: String,
    source: EffectiveBoundarySource,
    proof: BoundaryProof,
    comparison: Comparison,
) -> StackNetSelection {
    StackNetSelection {
        selected_tip: plan.selected_tip,
        frozen_layers: plan.frozen_layers,
        frozen_edges: plan.frozen_edges,
        effective_boundary: EffectiveBoundary::Proven {
            sha: boundary_sha,
            source,
            proof,
        },
        comparison: Some(comparison),
    }
}

/// Select one explicit path and compare exactly one proven effective boundary
/// to its frozen tip. No branch ref or current tracking ref is consulted.
pub fn select_local_stack_net(
    path: &Path,
    stack: &StackResolution,
    selected_tip: &StackPullRequestId,
) -> Result<StackNetSelection> {
    let plan = plan_stack_net(stack, selected_tip)?;
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
            && let Err(error) = prove_local_ancestor(path, merged_commit, &candidate.boundary_sha)
        {
            reasons.push(format!(
                "Lowest remaining base {} does not contain merged result {}: {error}",
                short_oid(&candidate.boundary_sha),
                short_oid(merged_commit)
            ));
            continue;
        }
        match prove_local_linear_pair(path, &candidate.boundary_sha, &tip)
            .and_then(|history| validate_remaining_layer_heads(&plan, &history))
        {
            Ok(()) => {
                let revision = Revision {
                    base_sha: candidate.boundary_sha.clone(),
                    head_sha: tip.clone(),
                };
                let comparison = local_comparison(path, &revision)?;
                return Ok(completed_selection(
                    plan,
                    candidate.boundary_sha.clone(),
                    candidate.source,
                    BoundaryProof::LocalDirectAncestor,
                    comparison,
                ));
            }
            Err(error) => reasons.push(format!(
                "Boundary {} is unavailable: {error}",
                short_oid(&candidate.boundary_sha)
            )),
        }
    }
    Ok(unavailable_selection(plan, reasons))
}

fn prove_local_ancestor(path: &Path, ancestor: &str, descendant: &str) -> Result<()> {
    validate_object_id(ancestor)?;
    validate_object_id(descendant)?;
    if ancestor == descendant {
        return Ok(());
    }
    let inventory = local_commit_inventory(
        path,
        &Revision {
            base_sha: ancestor.to_owned(),
            head_sha: descendant.to_owned(),
        },
    )?;
    ensure!(
        inventory.availability == InventoryAvailability::Complete,
        "Ancestry proof is incomplete"
    );
    let by_sha: HashMap<_, _> = inventory
        .commits
        .iter()
        .map(|entry| (entry.sha.as_str(), entry))
        .collect();
    let mut pending = vec![descendant];
    let mut seen = HashSet::new();
    while let Some(current) = pending.pop() {
        if !seen.insert(current) {
            continue;
        }
        let Some(entry) = by_sha.get(current) else {
            continue;
        };
        for parent in &entry.parent_shas {
            if parent == ancestor {
                return Ok(());
            }
            pending.push(parent);
        }
    }
    bail!("Required commit is not an ancestor of the frozen base")
}

fn prove_local_linear_pair(path: &Path, boundary: &str, tip: &str) -> Result<Vec<String>> {
    validate_object_id(boundary)?;
    validate_object_id(tip)?;
    if boundary == tip {
        return Ok(vec![boundary.to_owned()]);
    }
    let revision = Revision {
        base_sha: boundary.to_owned(),
        head_sha: tip.to_owned(),
    };
    let inventory = local_commit_inventory(path, &revision)?;
    ensure!(
        inventory.availability == InventoryAvailability::Complete,
        "Direct ancestry proof is incomplete"
    );
    let by_sha: HashMap<_, _> = inventory
        .commits
        .iter()
        .map(|entry| (entry.sha.as_str(), entry))
        .collect();
    ensure!(
        by_sha.len() == inventory.commits.len(),
        "Direct ancestry proof repeated a commit"
    );
    let mut current = tip;
    let mut visited = HashSet::new();
    let mut history = Vec::new();
    loop {
        let entry = by_sha
            .get(current)
            .context("Candidate boundary is not an ancestor of the selected tip")?;
        ensure!(
            visited.insert(current),
            "Direct ancestry proof contains a cycle"
        );
        history.push(current.to_owned());
        ensure!(
            entry.parent_shas.len() == 1,
            "Internal merge or root commit makes the direct path unprovable"
        );
        let parent = entry.parent_shas[0].as_str();
        if parent == boundary {
            break;
        }
        current = parent;
    }
    ensure!(
        visited.len() == inventory.commits.len(),
        "Direct ancestry proof contains commits outside one linear path"
    );
    history.push(boundary.to_owned());
    history.reverse();
    Ok(history)
}

fn valid_text_identity(value: &str) -> bool {
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

fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}
