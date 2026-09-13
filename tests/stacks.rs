use cibergit::{
    domain::{Account, Repository, Revision},
    stacks::{
        BoundaryProof, EffectiveBoundary, EffectiveBoundarySource, NativeStackAvailability,
        NativeStackRead, PERSONAL_CORRECTIONS_SCHEMA_VERSION, PersonalStackCorrections, StackEdge,
        StackEdgeProvenance, StackLayer, StackLayerState, StackPullRequestId, StackRef,
        StackRepository, resolve_stack, select_local_stack_net,
    },
};
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn repo() -> Repository {
    Repository {
        host: "github.com".into(),
        owner: "owner".into(),
        name: "repo".into(),
        account: Account {
            host: "github.com".into(),
            login: "alice".into(),
        },
        local_path: None,
    }
}

fn stack_repo() -> StackRepository {
    StackRepository::from_repository(&repo())
}

fn id(number: u64) -> StackPullRequestId {
    StackPullRequestId {
        repository: stack_repo(),
        number,
    }
}

fn oid(character: char) -> String {
    character.to_string().repeat(40)
}

fn layer(
    number: u64,
    source_branch: &str,
    target_branch: &str,
    base: String,
    head: String,
    state: StackLayerState,
) -> StackLayer {
    StackLayer {
        id: id(number),
        native_entry_id: None,
        native_position: None,
        source: StackRef {
            repository: Some(stack_repo()),
            branch: source_branch.into(),
            oid: head.clone(),
        },
        target: StackRef {
            repository: Some(stack_repo()),
            branch: target_branch.into(),
            oid: base.clone(),
        },
        revision: Revision {
            base_sha: base,
            head_sha: head,
        },
        state,
        merged_commit_oid: None,
    }
}

fn native(mut layers: Vec<StackLayer>, selected: u64) -> NativeStackRead {
    for (index, layer) in layers.iter_mut().enumerate() {
        layer.native_entry_id = Some(format!("entry-{}", layer.id.number));
        layer.native_position = Some(u32::try_from(index + 1).unwrap());
    }
    NativeStackRead {
        availability: NativeStackAvailability::Complete,
        stack_node_id: Some("stack-node".into()),
        stack_number: Some(7),
        stack_base_ref: Some("main".into()),
        selected_entry_id: Some(format!("entry-{selected}")),
        layers,
        notice: None,
    }
}

#[test]
fn native_relationships_win_and_personal_edge_is_explicit() {
    let first = layer(1, "one", "main", oid('a'), oid('b'), StackLayerState::Open);
    let second = layer(2, "two", "one", oid('b'), oid('c'), StackLayerState::Open);
    let third = layer(3, "three", "two", oid('c'), oid('d'), StackLayerState::Open);
    let native_read = native(vec![first.clone(), second.clone(), third.clone()], 2);
    let resolved = resolve_stack(&repo(), &id(2), &[], &native_read, None).unwrap();
    assert_eq!(resolved.edges.len(), 2);
    assert!(
        resolved
            .edges
            .iter()
            .all(|edge| edge.provenance == StackEdgeProvenance::Native)
    );

    let corrections = PersonalStackCorrections {
        schema_version: PERSONAL_CORRECTIONS_SCHEMA_VERSION,
        repository_key: repo().cache_key(),
        account: repo().account,
        edges: vec![StackEdge {
            parent: id(1),
            child: id(3),
            provenance: StackEdgeProvenance::Personal,
        }],
    };
    let corrected_native = native(native_read.layers.clone(), 3);
    let corrected =
        resolve_stack(&repo(), &id(3), &[], &corrected_native, Some(&corrections)).unwrap();
    assert!(corrected.edges.iter().any(|edge| {
        edge.parent == id(1)
            && edge.child == id(3)
            && edge.provenance == StackEdgeProvenance::Personal
    }));
    assert!(
        corrected
            .notice
            .as_deref()
            .unwrap()
            .contains("do not change remote")
    );
}

#[test]
fn inference_uses_repository_identity_and_allows_sibling_tips() {
    let root = layer(1, "root", "main", oid('a'), oid('b'), StackLayerState::Open);
    let left = layer(2, "left", "root", oid('b'), oid('c'), StackLayerState::Open);
    let right = layer(
        3,
        "right",
        "root",
        oid('b'),
        oid('d'),
        StackLayerState::Open,
    );
    let candidates = vec![root, left, right];
    let resolved = resolve_stack(
        &repo(),
        &id(2),
        &candidates,
        &NativeStackRead::not_member(),
        None,
    )
    .unwrap();
    assert_eq!(resolved.edges.len(), 2);
    assert!(
        resolved
            .edges
            .iter()
            .all(|edge| edge.provenance == StackEdgeProvenance::Inferred)
    );

    let mut foreign_root = candidates[0].clone();
    foreign_root.source.repository.as_mut().unwrap().owner = "fork".into();
    let disconnected = resolve_stack(
        &repo(),
        &id(2),
        &[foreign_root, candidates[1].clone()],
        &NativeStackRead::not_member(),
        None,
    )
    .unwrap();
    assert!(disconnected.edges.is_empty());
}

#[test]
fn duplicate_parent_cycle_and_foreign_correction_are_rejected() {
    let one = layer(
        1,
        "shared",
        "main",
        oid('a'),
        oid('b'),
        StackLayerState::Open,
    );
    let two = layer(
        2,
        "shared",
        "main",
        oid('a'),
        oid('c'),
        StackLayerState::Open,
    );
    let child = layer(
        3,
        "child",
        "shared",
        oid('b'),
        oid('d'),
        StackLayerState::Open,
    );
    assert!(
        resolve_stack(
            &repo(),
            &id(3),
            &[one.clone(), two, child],
            &NativeStackRead::not_member(),
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("multiple candidate parents")
    );

    let a = layer(1, "a", "b", oid('a'), oid('b'), StackLayerState::Open);
    let b = layer(2, "b", "a", oid('b'), oid('c'), StackLayerState::Open);
    assert!(
        resolve_stack(
            &repo(),
            &id(2),
            &[a, b],
            &NativeStackRead::not_member(),
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("cycle")
    );

    let corrections = PersonalStackCorrections {
        schema_version: PERSONAL_CORRECTIONS_SCHEMA_VERSION,
        repository_key: repo().cache_key(),
        account: Account {
            host: "github.com".into(),
            login: "other".into(),
        },
        edges: vec![],
    };
    assert!(
        resolve_stack(
            &repo(),
            &id(1),
            &[one],
            &NativeStackRead::not_member(),
            Some(&corrections),
        )
        .unwrap_err()
        .to_string()
        .contains("different account")
    );
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn init() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "--initial-branch=main"]);
    git(dir.path(), &["config", "user.name", "Stack Test"]);
    git(dir.path(), &["config", "user.email", "stack@example.test"]);
    dir
}

fn commit_file(path: &Path, name: &str, contents: &[u8], message: &str) -> String {
    fs::write(path.join(name), contents).unwrap();
    git(path, &["add", "--", name]);
    git(path, &["commit", "-q", "-m", message]);
    git(path, &["rev-parse", "HEAD"])
}

fn inferred(layers: &[StackLayer], selected: u64) -> cibergit::stacks::StackResolution {
    resolve_stack(
        &repo(),
        &id(selected),
        layers,
        &NativeStackRead::not_member(),
        None,
    )
    .unwrap()
}

#[test]
fn two_layers_return_one_net_tree_diff_without_patch_concatenation() {
    let dir = init();
    let base = commit_file(dir.path(), "story.txt", b"base\n", "base");
    let lower = commit_file(dir.path(), "story.txt", b"base\nlower\n", "lower");
    let tip = commit_file(dir.path(), "story.txt", b"base\nlower\nupper\n", "upper");
    let layers = vec![
        layer(
            1,
            "lower",
            "main",
            base.clone(),
            lower.clone(),
            StackLayerState::Open,
        ),
        layer(
            2,
            "upper",
            "lower",
            lower,
            tip.clone(),
            StackLayerState::Open,
        ),
    ];
    let snapshot = native(layers.clone(), 2);
    let resolved = resolve_stack(&repo(), &id(2), &[], &snapshot, None).unwrap();
    let selection = select_local_stack_net(dir.path(), &resolved, &id(2)).unwrap();
    assert_eq!(
        selection.comparison.as_ref().unwrap().revision.head_sha,
        tip
    );
    assert_eq!(selection.comparison.as_ref().unwrap().files.len(), 1);
    let patch = selection.comparison.unwrap().files.remove(0).patch.unwrap();
    assert_eq!(patch.matches("+lower").count(), 1);
    assert_eq!(patch.matches("+upper").count(), 1);
}

#[test]
fn advanced_lower_head_cannot_be_omitted_from_a_proven_net_diff() {
    let dir = init();
    let base = commit_file(dir.path(), "base.txt", b"base\n", "base");
    let first_lower = commit_file(dir.path(), "lower.txt", b"first\n", "first lower");
    let advanced_lower = commit_file(dir.path(), "required.txt", b"required\n", "lower advance");
    git(dir.path(), &["checkout", "-q", "--detach", &first_lower]);
    let tip = commit_file(dir.path(), "upper.txt", b"upper\n", "upper");
    let layers = vec![
        layer(
            1,
            "lower",
            "main",
            base,
            advanced_lower.clone(),
            StackLayerState::Open,
        ),
        layer(
            2,
            "upper",
            "lower",
            advanced_lower,
            tip,
            StackLayerState::Open,
        ),
    ];
    let native_read = native(layers.clone(), 2);
    let native_resolution = resolve_stack(&repo(), &id(2), &[], &native_read, None).unwrap();
    let corrections = PersonalStackCorrections {
        schema_version: PERSONAL_CORRECTIONS_SCHEMA_VERSION,
        repository_key: repo().cache_key(),
        account: repo().account,
        edges: vec![StackEdge {
            parent: id(1),
            child: id(2),
            provenance: StackEdgeProvenance::Personal,
        }],
    };
    let personal = resolve_stack(
        &repo(),
        &id(2),
        &layers,
        &NativeStackRead::not_member(),
        Some(&corrections),
    )
    .unwrap();
    for resolution in [inferred(&layers, 2), native_resolution, personal] {
        let result = select_local_stack_net(dir.path(), &resolution, &id(2)).unwrap();
        assert!(
            matches!(result.effective_boundary, EffectiveBoundary::Unavailable { ref reason }
            if reason.contains("head is absent")),
            "A proven net diff must not omit the frozen lower layer's required.txt change"
        );
        assert!(result.comparison.is_none());
    }
}

#[test]
fn merged_lower_original_head_becomes_the_boundary() {
    let dir = init();
    let base = commit_file(dir.path(), "lower.txt", b"base\n", "base");
    let lower = commit_file(dir.path(), "lower.txt", b"merged\n", "lower");
    let tip = commit_file(dir.path(), "upper.txt", b"remaining\n", "upper");
    let mut first = layer(
        1,
        "lower",
        "main",
        base,
        lower.clone(),
        StackLayerState::Merged,
    );
    first.merged_commit_oid = Some(lower.clone());
    let layers = vec![
        first,
        layer(
            2,
            "upper",
            "lower",
            lower.clone(),
            tip,
            StackLayerState::Open,
        ),
    ];
    let snapshot = native(layers.clone(), 2);
    let resolved = resolve_stack(&repo(), &id(2), &[], &snapshot, None).unwrap();
    let selection = select_local_stack_net(dir.path(), &resolved, &id(2)).unwrap();
    assert!(matches!(
        selection.effective_boundary,
        EffectiveBoundary::Proven {
            sha,
            source: EffectiveBoundarySource::MergedLayerHead { .. },
            proof: BoundaryProof::LocalDirectAncestor,
        } if sha == lower
    ));
    let files = &selection.comparison.unwrap().files;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "upper.txt");
}

#[test]
fn squash_merge_fallback_requires_and_proves_the_actual_merge_result() {
    let dir = init();
    let base = commit_file(dir.path(), "base.txt", b"base\n", "base");
    git(dir.path(), &["switch", "-q", "-c", "lower"]);
    let lower = commit_file(dir.path(), "lower.txt", b"lower\n", "lower");
    git(dir.path(), &["switch", "-q", "main"]);
    fs::write(dir.path().join("lower.txt"), b"lower\n").unwrap();
    git(dir.path(), &["add", "lower.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "squash lower"]);
    let squash = git(dir.path(), &["rev-parse", "HEAD"]);
    let tip = commit_file(dir.path(), "upper.txt", b"upper\n", "rebased upper");

    let mut merged = layer(1, "lower", "main", base, lower, StackLayerState::Merged);
    merged.merged_commit_oid = Some(squash.clone());
    let layers = vec![
        merged.clone(),
        layer(
            2,
            "upper",
            "main",
            squash.clone(),
            tip,
            StackLayerState::Open,
        ),
    ];
    let snapshot = native(layers.clone(), 2);
    let resolved = resolve_stack(&repo(), &id(2), &[], &snapshot, None).unwrap();
    let selection = select_local_stack_net(dir.path(), &resolved, &id(2)).unwrap();
    assert!(
        matches!(
            &selection.effective_boundary,
            EffectiveBoundary::Proven {
                sha,
                source: EffectiveBoundarySource::LowestRemainingTarget { .. },
                ..
            } if sha == &squash
        ),
        "{:?}",
        selection.effective_boundary
    );
    assert_eq!(selection.comparison.as_ref().unwrap().files.len(), 1);
    assert_eq!(
        selection.comparison.as_ref().unwrap().files[0].path,
        "upper.txt"
    );

    merged.merged_commit_oid = None;
    let missing = vec![merged, layers[1].clone()];
    let missing_snapshot = native(missing, 2);
    let missing_resolved = resolve_stack(&repo(), &id(2), &[], &missing_snapshot, None).unwrap();
    let unavailable = select_local_stack_net(dir.path(), &missing_resolved, &id(2)).unwrap();
    assert!(matches!(
        unavailable.effective_boundary,
        EffectiveBoundary::Unavailable { ref reason }
            if reason.contains("no provider-observed merge commit")
    ));
    assert!(unavailable.comparison.is_none());
}

#[test]
fn closed_unmerged_remains_and_all_merged_is_explicitly_empty() {
    let dir = init();
    let base = commit_file(dir.path(), "base.txt", b"base\n", "base");
    let closed = commit_file(dir.path(), "closed.txt", b"closed\n", "closed");
    let tip = commit_file(dir.path(), "tip.txt", b"tip\n", "tip");
    let layers = vec![
        layer(
            1,
            "closed",
            "main",
            base.clone(),
            closed.clone(),
            StackLayerState::ClosedUnmerged,
        ),
        layer(
            2,
            "tip",
            "closed",
            closed,
            tip.clone(),
            StackLayerState::Open,
        ),
    ];
    let selected = select_local_stack_net(dir.path(), &inferred(&layers, 2), &id(2)).unwrap();
    assert_eq!(
        selected.frozen_layers[0].state,
        StackLayerState::ClosedUnmerged
    );
    assert_eq!(
        selected.comparison.unwrap().revision,
        Revision {
            base_sha: base.clone(),
            head_sha: tip.clone()
        }
    );

    let mut mixed = layers.clone();
    mixed[1].state = StackLayerState::Merged;
    mixed[1].merged_commit_oid = Some(tip);
    let mixed = select_local_stack_net(dir.path(), &inferred(&mixed, 2), &id(2)).unwrap();
    assert!(matches!(
        mixed.effective_boundary,
        EffectiveBoundary::Unavailable { ref reason }
            if reason.contains("merged descendant follows an unmerged dependency")
    ));

    let mut merged = layers;
    for layer in &mut merged {
        layer.state = StackLayerState::Merged;
        layer.merged_commit_oid = Some(layer.revision.head_sha.clone());
    }
    let empty = select_local_stack_net(dir.path(), &inferred(&merged, 2), &id(2)).unwrap();
    assert_eq!(empty.frozen_layers.len(), 2);
    assert!(matches!(
        empty.effective_boundary,
        EffectiveBoundary::AllMerged
    ));
    assert!(empty.comparison.is_none());
}

#[test]
fn internal_merge_is_refused_and_binary_file_remains_listed() {
    let dir = init();
    let base = commit_file(dir.path(), "base.txt", b"base\n", "base");
    git(dir.path(), &["switch", "-q", "-c", "side"]);
    let _side = commit_file(dir.path(), "side.txt", b"side\n", "side");
    git(dir.path(), &["switch", "-q", "main"]);
    let _main = commit_file(dir.path(), "main.txt", b"main\n", "main");
    git(
        dir.path(),
        &["merge", "-q", "--no-ff", "side", "-m", "merge side"],
    );
    let merged_tip = git(dir.path(), &["rev-parse", "HEAD"]);
    let merged_layer = layer(
        1,
        "feature",
        "main",
        base.clone(),
        merged_tip,
        StackLayerState::Open,
    );
    let unavailable =
        select_local_stack_net(dir.path(), &inferred(&[merged_layer], 1), &id(1)).unwrap();
    assert!(matches!(
        unavailable.effective_boundary,
        EffectiveBoundary::Unavailable { ref reason } if reason.contains("Internal merge")
    ));

    git(dir.path(), &["reset", "-q", "--hard", &base]);
    let binary_tip = commit_file(dir.path(), "image.bin", &[0, 1, 0, 2, 255], "binary");
    let binary_layer = layer(
        2,
        "binary",
        "main",
        base.clone(),
        binary_tip.clone(),
        StackLayerState::Open,
    );
    let binary = select_local_stack_net(dir.path(), &inferred(&[binary_layer], 2), &id(2)).unwrap();
    let comparison = binary.comparison.unwrap();
    assert_eq!(
        comparison.revision,
        Revision {
            base_sha: base,
            head_sha: binary_tip
        }
    );
    assert_eq!(comparison.files.len(), 1);
    assert_eq!(comparison.files[0].path, "image.bin");
    assert!(comparison.files[0].patch.is_none());
    assert!(!comparison.files[0].patch_complete);
}
