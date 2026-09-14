use cibergit::{
    domain::{
        Account, ChangedFile, Comparison, MergeEligibility, PendingFileCommentSource,
        PendingFileReviewAbsence, PendingReviewSnapshot, ProviderCoordinates, PullRequestDetails,
        PullRequestReview, ReviewComment, ReviewSubject, ReviewThread, Revision,
    },
    participation::{
        CanonicalPublishedPatch, DiffSide, DraftDisposition, DraftStore, LineSelection,
        LoadOutcome, MAX_DRAFT_TEXT_BYTES, MAX_DRAFTS, MAX_OPERATIONS, MappingIssue,
        ParticipationError, ReviewCommentTarget, ReviewComposition, ReviewEvent, ReviewKey,
        ReviewOperationStatus, map_file_to_canonical_published, map_to_canonical_published,
        validate_coordinate,
    },
    review::{ComparisonMetadata, ComparisonMode, ReviewSession, file_key},
};
use std::{fs, os::unix::fs::PermissionsExt};
use tempfile::tempdir;

const PATCH: &str = "@@ -10,4 +10,5 @@\n context\n-old\n+new\n keep\n+added\n tail\n";

fn revision(base: &str, head: &str) -> Revision {
    Revision {
        base_sha: base.into(),
        head_sha: head.into(),
    }
}

fn file(path: &str) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        previous_path: None,
        raw_path: None,
        raw_previous_path: None,
        status: "modified".into(),
        additions: 2,
        deletions: 1,
        patch: Some(PATCH.into()),
        patch_complete: true,
    }
}

fn comparison(base: &str, head: &str, file: ChangedFile) -> Comparison {
    Comparison {
        revision: revision(base, head),
        files: vec![file],
        complete: true,
        notice: None,
    }
}

fn published(comparison: &Comparison) -> CanonicalPublishedPatch<'_> {
    CanonicalPublishedPatch::new(comparison, &ComparisonMetadata::default()).unwrap()
}

fn key(login: &str) -> ReviewKey {
    ReviewKey {
        provider: "github".into(),
        host: "github.com".into(),
        owner: "acme".into(),
        repository: "rocket".into(),
        account: Account {
            host: "github.com".into(),
            login: login.into(),
        },
        pull_request: 42,
    }
}

fn coordinate_for(
    comparison: Comparison,
    side: DiffSide,
    start_line: u64,
    line: u64,
) -> cibergit::participation::DraftCoordinate {
    let selected = file_key(&comparison.files[0]);
    let session = ReviewSession::new(comparison);
    validate_coordinate(
        &session,
        &selected,
        LineSelection {
            side,
            start_line,
            line,
        },
    )
    .unwrap()
}

fn provider_coordinates(remote_id: &str) -> ProviderCoordinates {
    ProviderCoordinates {
        provider: "github".into(),
        host: "github.com".into(),
        owner: "acme".into(),
        repository: "rocket".into(),
        pull_request: 42,
        remote_id: remote_id.into(),
    }
}

fn details(
    pending_id: Option<&str>,
    comment_id: Option<&str>,
    comment_body: &str,
) -> PullRequestDetails {
    let reviews = pending_id
        .map(|id| PullRequestReview {
            coordinates: provider_coordinates(id),
            author: Some("alice".into()),
            body: String::new(),
            state: "PENDING".into(),
            submitted_at: None,
            commit_sha: Some("head".into()),
            edit_summary_capability: None,
            dismissal_capability: None,
            url: String::new(),
        })
        .into_iter()
        .collect();
    let review_threads = comment_id
        .map(|id| ReviewThread {
            coordinates: provider_coordinates("thread-1"),
            path: "src/lib.rs".into(),
            subject: cibergit::domain::ReviewSubject::Line,
            line: Some(11),
            original_line: None,
            start_line: None,
            original_start_line: None,
            side: Some("RIGHT".into()),
            start_side: None,
            resolved: false,
            outdated: false,
            comments: vec![ReviewComment {
                coordinates: provider_coordinates(id),
                author: Some("alice".into()),
                body: comment_body.into(),
                created_at: String::new(),
                updated_at: String::new(),
                url: String::new(),
                path: "src/lib.rs".into(),
                subject: cibergit::domain::ReviewSubject::Line,
                line: Some(11),
                original_line: None,
                start_line: None,
                original_start_line: None,
                side: Some("RIGHT".into()),
                diff_hunk: PATCH.into(),
                commit_sha: Some("head".into()),
                original_commit_sha: Some("head".into()),
                outdated: false,
            }],
            comments_complete: true,
        })
        .into_iter()
        .collect();
    PullRequestDetails {
        number: 42,
        pull_request_node_id: None,
        base_repository: None,
        observed_head_sha: None,
        rollup_commit_sha: None,
        potential_merge_commit_sha: None,
        head_repository: None,
        rollup_repository: None,
        potential_merge_commit_repository: None,
        body: String::new(),
        requested_reviewers: vec![],
        labels: vec![],
        assignees: vec![],
        merge_eligibility: MergeEligibility {
            state: "OPEN".into(),
            draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state_status: "CLEAN".into(),
            review_status: String::new(),
            check_status: String::new(),
            maintainer_can_modify: true,
            can_rebase: true,
            can_update_branch: false,
            auto_merge_enabled: false,
            in_merge_queue: false,
        },
        issue_comments: vec![],
        reviews,
        review_threads,
        reactions: vec![],
        checks: vec![],
        activity_complete: true,
        checks_complete: true,
        notice: None,
    }
}

#[test]
fn validates_old_new_context_changed_and_multiline_coordinates_from_parser_snapshot() {
    let comparison = comparison("base", "head", file("src/lib.rs"));
    let old_deletion = coordinate_for(comparison.clone(), DiffSide::Old, 11, 11);
    assert_eq!(old_deletion.anchors[0].text, "old");
    assert_eq!(
        old_deletion.anchors[0].kind,
        cibergit::participation::CoordinateLineKind::Deletion
    );

    let new_addition = coordinate_for(comparison.clone(), DiffSide::New, 11, 11);
    assert_eq!(new_addition.anchors[0].text, "new");
    assert_eq!(
        new_addition.anchors[0].kind,
        cibergit::participation::CoordinateLineKind::Addition
    );

    let context = coordinate_for(comparison.clone(), DiffSide::Old, 10, 10);
    assert_eq!(context.anchors[0].text, "context");
    let multiline = coordinate_for(comparison.clone(), DiffSide::New, 10, 14);
    assert_eq!(multiline.anchors.len(), 5);
    assert_eq!(multiline.anchors[4].text, "tail");

    let session = ReviewSession::new(comparison);
    let error = validate_coordinate(
        &session,
        "src/lib.rs",
        LineSelection {
            side: DiffSide::Old,
            start_line: 9,
            line: 11,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("line 9 is not selectable"));
}

#[test]
fn preserves_rename_and_raw_identity_and_refuses_non_utf8_provider_path() {
    let mut renamed = file("src/new.rs");
    renamed.previous_path = Some("src/old.rs".into());
    renamed.status = "renamed".into();
    let canonical = comparison("base", "head", renamed.clone());
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mapped = map_to_canonical_published(&coordinate, published(&canonical)).unwrap();
    assert_eq!(mapped.path, "src/new.rs");
    assert_eq!(coordinate.previous_path.as_deref(), Some("src/old.rs"));

    let mut raw = renamed;
    raw.path = "src/bad\\xff.rs".into();
    raw.raw_path = Some(b"src/bad\xff.rs".to_vec());
    raw.raw_previous_path = Some(b"src/old\xfe.rs".to_vec());
    raw.previous_path = Some("src/old\\xfe.rs".into());
    let raw_comparison = comparison("base", "head", raw);
    let raw_coordinate = coordinate_for(raw_comparison.clone(), DiffSide::New, 11, 11);
    assert!(raw_coordinate.file_key.starts_with("\0raw:"));
    assert_eq!(
        map_to_canonical_published(&raw_coordinate, published(&raw_comparison)),
        Err(MappingIssue::RawPathUnsupported)
    );
}

#[test]
fn commit_range_coordinate_requires_exact_canonical_patch_verification() {
    let active = comparison("range-base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(active.clone(), DiffSide::New, 11, 12);
    let canonical = comparison("pr-base", "head", file("src/lib.rs"));
    let active_metadata = ComparisonMetadata {
        mode: ComparisonMode::CommitRange,
        requested_mode: None,
        notice: None,
    };
    assert_eq!(
        CanonicalPublishedPatch::new(&canonical, &active_metadata).unwrap_err(),
        MappingIssue::NotCanonicalPublishedPatch
    );
    let mapped = map_to_canonical_published(&coordinate, published(&canonical)).unwrap();
    assert_eq!(mapped.start_line, Some(11));
    assert_eq!(mapped.line, 12);

    let old_coordinate = coordinate_for(active, DiffSide::Old, 10, 11);
    assert!(matches!(
        map_to_canonical_published(&old_coordinate, published(&canonical)),
        Err(MappingIssue::OldSideBaseMismatch {
            reviewed_base,
            canonical_base,
        }) if reviewed_base == "range-base" && canonical_base == "pr-base"
    ));

    let mut changed = file("src/lib.rs");
    changed.patch = Some(PATCH.replace("+new", "+different"));
    let mismatch = comparison("pr-base", "head", changed);
    assert!(matches!(
        map_to_canonical_published(&coordinate, published(&mismatch)),
        Err(MappingIssue::LineChanged { line: 11, .. })
    ));

    let newer = comparison("pr-base", "new-head", file("src/lib.rs"));
    assert!(matches!(
        map_to_canonical_published(&coordinate, published(&newer)),
        Err(MappingIssue::OutdatedRevision { .. })
    ));
}

#[test]
fn dirty_local_text_survives_remote_pending_refresh_while_clean_text_updates() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let first = state
        .add_draft(coordinate.clone(), "local one")
        .unwrap()
        .id
        .clone();
    let first_intent = state
        .prepare_pending_comment(&first, published(&canonical))
        .unwrap();
    state
        .mark_in_flight(&first_intent.operation_id, "attempt-1")
        .unwrap();
    state
        .reconcile_observed_comment_success(
            &first_intent.operation_id,
            Some("pending-1".into()),
            "comment-1".into(),
            "local one".into(),
        )
        .unwrap();
    state.edit_draft(&first, "dirty local one").unwrap();

    let second = state.add_draft(coordinate, "local two").unwrap().id.clone();
    let second_intent = state
        .prepare_pending_comment(&second, published(&canonical))
        .unwrap();
    state
        .mark_in_flight(&second_intent.operation_id, "attempt-2")
        .unwrap();
    state
        .reconcile_observed_comment_success(
            &second_intent.operation_id,
            Some("pending-1".into()),
            "comment-2".into(),
            "local two".into(),
        )
        .unwrap();

    let mut remote = details(Some("pending-1"), Some("comment-1"), "remote first");
    remote.review_threads[0].comments.push(ReviewComment {
        coordinates: provider_coordinates("comment-2"),
        author: Some("alice".into()),
        body: "remote second".into(),
        created_at: String::new(),
        updated_at: String::new(),
        url: String::new(),
        path: "src/lib.rs".into(),
        subject: cibergit::domain::ReviewSubject::Line,
        line: Some(11),
        original_line: None,
        start_line: None,
        original_start_line: None,
        side: Some("RIGHT".into()),
        diff_hunk: PATCH.into(),
        commit_sha: Some("head".into()),
        original_commit_sha: Some("head".into()),
        outdated: false,
    });
    let report = state.reconcile_remote_pending(&remote).unwrap();
    assert_eq!(report.dirty_drafts_preserved, 1);
    assert_eq!(report.clean_drafts_updated, 1);
    assert_eq!(state.draft(&first).unwrap().body, "dirty local one");
    assert_eq!(
        state.draft(&first).unwrap().observed_remote_body.as_deref(),
        Some("remote first")
    );
    assert_eq!(state.draft(&second).unwrap().body, "remote second");

    state
        .reconcile_remote_pending(&details(None, None, ""))
        .unwrap();
    assert_eq!(state.observed_pending_review_id, None);
    assert_eq!(
        state.acknowledged_pending_review_id.as_deref(),
        Some("pending-1")
    );
    assert_eq!(state.draft(&first).unwrap().body, "dirty local one");
}

#[test]
fn pending_is_default_and_submission_and_immediate_post_are_explicit() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let draft = state
        .add_draft(coordinate.clone(), "pending")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        state.draft(&draft).unwrap().disposition,
        DraftDisposition::Pending
    );
    assert!(matches!(
        state.prepare_submission(ReviewEvent::Approve, "looks good", Some("head")),
        Err(ParticipationError::UnsynchronizedDrafts { count: 1 })
    ));
    let pending = state
        .prepare_pending_comment(&draft, published(&canonical))
        .unwrap();
    state
        .mark_in_flight(&pending.operation_id, "attempt")
        .unwrap();
    state
        .reconcile_observed_comment_success(
            &pending.operation_id,
            Some("pending-1".into()),
            "comment-1".into(),
            "pending".into(),
        )
        .unwrap();
    let submission = state
        .prepare_submission(ReviewEvent::Approve, "looks good", Some("newer-head"))
        .unwrap();
    assert_eq!(submission.reviewed_commit_sha, "head");
    assert!(
        submission
            .newer_head_warning
            .as_deref()
            .unwrap()
            .contains("newer-head")
    );
    assert!(
        submission
            .newer_head_warning
            .as_deref()
            .unwrap()
            .contains("head")
    );

    let mut immediate_state =
        ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let immediate_draft = immediate_state
        .add_draft(coordinate, "now")
        .unwrap()
        .id
        .clone();
    let immediate = immediate_state
        .prepare_immediate_comment(&immediate_draft, published(&canonical))
        .unwrap();
    assert_eq!(immediate.body, "now");
    assert_eq!(immediate.position.commit_sha, "head");
}

#[test]
fn restart_preserves_uncertain_outcome_and_never_makes_it_replayable() {
    let directory = tempdir().unwrap();
    let store = DraftStore::open(directory.path()).unwrap();
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let draft = state
        .add_draft(coordinate, "maybe posted")
        .unwrap()
        .id
        .clone();
    let intent = state
        .prepare_pending_comment(&draft, published(&canonical))
        .unwrap();
    state
        .mark_in_flight(&intent.operation_id, "request-token")
        .unwrap();
    state
        .mark_uncertain(&intent.operation_id, "connection closed after upload")
        .unwrap();
    store.save(&state).unwrap();

    let mut restored = match store.load(&key("alice")).unwrap() {
        LoadOutcome::Loaded(state) => *state,
        other => panic!("unexpected load outcome: {other:?}"),
    };
    let operation = restored
        .operations_requiring_reconciliation()
        .next()
        .unwrap();
    assert!(matches!(
        operation.status,
        ReviewOperationStatus::Uncertain { .. }
    ));
    assert!(matches!(
        restored.prepare_pending_comment(&draft, published(&canonical)),
        Err(ParticipationError::OperationNeedsReconciliation(_))
    ));
    assert_eq!(restored.draft(&draft).unwrap().body, "maybe posted");
    restored
        .reconcile_observed_not_applied(&intent.operation_id, "provider lookup found no comment")
        .unwrap();
    let explicit_retry = restored
        .prepare_pending_comment(&draft, published(&canonical))
        .unwrap();
    assert_ne!(explicit_retry.operation_id, intent.operation_id);
}

#[test]
fn durable_store_partitions_accounts_and_preserves_corrupt_and_future_files() {
    let directory = tempdir().unwrap();
    let store = DraftStore::open(directory.path()).unwrap();
    let alice = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    store.save(&alice).unwrap();
    let alice_path = store.record_path(&key("alice")).unwrap();
    let bob_path = store.record_path(&key("bob")).unwrap();
    assert_ne!(alice_path, bob_path);
    assert!(matches!(
        store.load(&key("alice")).unwrap(),
        LoadOutcome::Loaded(_)
    ));
    assert!(matches!(
        store.load(&key("bob")).unwrap(),
        LoadOutcome::Missing
    ));
    assert_eq!(
        fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&alice_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let original_corrupt = b"{not json";
    fs::write(&alice_path, original_corrupt).unwrap();
    assert!(matches!(
        store.load(&key("alice")).unwrap(),
        LoadOutcome::Corrupt { .. }
    ));
    assert!(matches!(
        store.save(&alice),
        Err(ParticipationError::RecoveryRequired { .. })
    ));
    assert_eq!(fs::read(&alice_path).unwrap(), original_corrupt);

    fs::write(&alice_path, br#"{"version":99,"state":{}}"#).unwrap();
    assert!(matches!(
        store.load(&key("alice")).unwrap(),
        LoadOutcome::FutureVersion { version: 99, .. }
    ));
    assert!(matches!(
        store.save(&alice),
        Err(ParticipationError::RecoveryRequired { .. })
    ));
    assert_eq!(
        fs::read_to_string(&alice_path).unwrap(),
        r#"{"version":99,"state":{}}"#
    );
}

#[test]
fn text_draft_and_operation_bounds_return_specific_errors() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    assert!(matches!(
        state.add_draft(coordinate.clone(), "x".repeat(MAX_DRAFT_TEXT_BYTES + 1)),
        Err(ParticipationError::TextTooLarge {
            field: "draft text",
            ..
        })
    ));
    for _ in 0..MAX_DRAFTS {
        state.add_draft(coordinate.clone(), "bounded").unwrap();
    }
    assert!(matches!(
        state.add_draft(coordinate.clone(), "one too many"),
        Err(ParticipationError::TooManyDrafts { limit }) if limit == MAX_DRAFTS
    ));

    let mut operations = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let draft = operations
        .add_draft(coordinate, "sync repeatedly")
        .unwrap()
        .id
        .clone();
    for number in 0..MAX_OPERATIONS {
        let intent = operations
            .prepare_pending_comment(&draft, published(&canonical))
            .unwrap();
        operations
            .mark_in_flight(&intent.operation_id, format!("attempt-{number}"))
            .unwrap();
        operations
            .reconcile_observed_comment_success(
                &intent.operation_id,
                Some("pending-1".into()),
                format!("comment-{number}"),
                "sync repeatedly".into(),
            )
            .unwrap();
    }
    assert!(matches!(
        operations.prepare_pending_comment(&draft, published(&canonical)),
        Err(ParticipationError::TooManyOperations { limit }) if limit == MAX_OPERATIONS
    ));
}

#[test]
fn submitted_review_retires_pending_ids_and_preserves_edits_for_a_new_review() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let first = state
        .add_draft(coordinate.clone(), "first")
        .unwrap()
        .id
        .clone();
    let second = state.add_draft(coordinate, "second").unwrap().id.clone();
    for (id, body) in [(&first, "first"), (&second, "second")] {
        let intent = state
            .prepare_pending_comment(id, published(&canonical))
            .unwrap();
        state.mark_in_flight(&intent.operation_id, "sync").unwrap();
        state
            .reconcile_observed_comment_success(
                &intent.operation_id,
                Some("pending-1".into()),
                format!("remote-{id}"),
                body.into(),
            )
            .unwrap();
    }
    state.observed_pending_review_id = Some("pending-1".into());
    let submit = state
        .prepare_submission(ReviewEvent::Approve, "review summary", None)
        .unwrap();
    state
        .mark_in_flight(&submit.operation_id, "submit")
        .unwrap();
    state.edit_draft(&second, "a later thought").unwrap();
    assert!(
        state
            .prepare_pending_comment(&second, published(&canonical))
            .is_err()
    );
    assert!(
        state
            .reconcile_observed_submission_success(&submit.operation_id, "foreign-review".into())
            .is_err()
    );
    state
        .reconcile_observed_submission_success(&submit.operation_id, "pending-1".into())
        .unwrap();
    assert!(state.observed_pending_review_id.is_none());
    assert!(state.acknowledged_pending_review_id.is_none());
    assert_eq!(
        state.draft(&first).unwrap().disposition,
        DraftDisposition::Submitted
    );
    assert!(
        state
            .prepare_pending_comment(&first, published(&canonical))
            .is_err()
    );
    assert_eq!(state.draft(&second).unwrap().body, "a later thought");
    assert!(state.draft(&second).unwrap().remote.is_none());
    let next = state
        .prepare_pending_comment(&second, published(&canonical))
        .unwrap();
    assert!(next.pending_review_id.is_none());
    assert!(next.existing_comment_id.is_none());
}

#[test]
fn operation_payload_survives_draft_edit_timeout_and_restart() {
    use cibergit::participation::ReviewOperationPayload;
    let directory = tempdir().unwrap();
    let store = DraftStore::open(directory.path()).unwrap();
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let draft = state
        .add_draft(coordinate, "actually sent")
        .unwrap()
        .id
        .clone();
    let intent = state
        .prepare_immediate_comment(&draft, published(&canonical))
        .unwrap();
    state
        .mark_in_flight(&intent.operation_id, "attempt-1")
        .unwrap();
    state.edit_draft(&draft, "unsent later edit").unwrap();
    state
        .mark_uncertain(&intent.operation_id, "lost reply")
        .unwrap();
    store.save(&state).unwrap();
    let LoadOutcome::Loaded(mut restored) = store.load(&key("alice")).unwrap() else {
        panic!("missing recovery")
    };
    assert_eq!(
        restored.operations[0].payload,
        Some(ReviewOperationPayload::ImmediateComment(intent.clone()))
    );
    assert_eq!(restored.draft(&draft).unwrap().body, "unsent later edit");
    assert!(
        restored
            .prepare_submission(ReviewEvent::Comment, "summary", None)
            .is_err()
    );
    restored
        .reconcile_observed_comment_success(
            &intent.operation_id,
            None,
            "posted-1".into(),
            "actually sent".into(),
        )
        .unwrap();
    assert_eq!(restored.draft(&draft).unwrap().body, "unsent later edit");
    assert!(restored.draft(&draft).unwrap().dirty);
    assert!(
        restored
            .prepare_pending_comment(&draft, published(&canonical))
            .is_err()
    );
    assert!(
        restored
            .prepare_immediate_comment(&draft, published(&canonical))
            .is_err()
    );
}

#[test]
fn late_comment_ack_cannot_resurrect_review_submitted_in_browser() {
    for edit_while_in_flight in [false, true] {
        let directory = tempdir().unwrap();
        let store = DraftStore::open(directory.path()).unwrap();
        let canonical = comparison("base", "head", file("src/lib.rs"));
        let coordinate = coordinate_for(canonical.clone(), DiffSide::New, 11, 11);
        let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
        let draft = state.add_draft(coordinate, "sent text").unwrap().id.clone();
        let intent = state
            .prepare_pending_comment(&draft, published(&canonical))
            .unwrap();
        state
            .mark_in_flight(&intent.operation_id, "posting")
            .unwrap();
        if edit_while_in_flight {
            state.edit_draft(&draft, "unsent new text").unwrap();
        }
        let mut terminal = details(Some("review-1"), None, "");
        terminal.reviews[0].state = "COMMENTED".into();
        state.reconcile_remote_pending(&terminal).unwrap();
        store.save(&state).unwrap();
        let LoadOutcome::Loaded(mut state) = store.load(&key("alice")).unwrap() else {
            panic!("recovery missing")
        };
        state
            .reconcile_observed_comment_success(
                &intent.operation_id,
                Some("review-1".into()),
                "comment-1".into(),
                "sent text".into(),
            )
            .unwrap();
        // An older in-flight metadata read also cannot reinstate PENDING.
        state
            .reconcile_remote_pending(&details(Some("review-1"), None, ""))
            .unwrap();
        assert!(state.retired_review_ids.contains("review-1"));
        assert!(state.observed_pending_review_id.is_none());
        assert!(state.acknowledged_pending_review_id.is_none());
        if edit_while_in_flight {
            assert_eq!(state.draft(&draft).unwrap().body, "unsent new text");
            assert!(state.draft(&draft).unwrap().remote.is_none());
            let next = state
                .prepare_pending_comment(&draft, published(&canonical))
                .unwrap();
            assert!(next.pending_review_id.is_none());
            assert!(next.existing_comment_id.is_none());
        } else {
            assert_eq!(
                state.draft(&draft).unwrap().disposition,
                DraftDisposition::Submitted
            );
            assert!(
                state
                    .prepare_pending_comment(&draft, published(&canonical))
                    .is_err()
            );
        }
        store.save(&state).unwrap();
    }
}

fn repository(login: &str) -> cibergit::domain::Repository {
    cibergit::domain::Repository {
        host: "github.com".into(),
        owner: "acme".into(),
        name: "rocket".into(),
        account: Account {
            host: "github.com".into(),
            login: login.into(),
        },
        local_path: None,
    }
}

fn pending_file_source(base: &str, head: &str) -> PendingFileCommentSource {
    PendingFileCommentSource {
        viewer_login: "alice".into(),
        repository: repository("alice"),
        pull_request: provider_coordinates("PR_42"),
        pull_request_state: "OPEN".into(),
        current_base_sha: base.into(),
        current_head_sha: head.into(),
        review: provider_coordinates("REVIEW_pending"),
        review_author: "alice".into(),
        review_commit_sha: head.into(),
    }
}

fn pending_file_absence(base: &str, head: &str) -> PendingFileReviewAbsence {
    PendingFileReviewAbsence {
        viewer_login: "alice".into(),
        repository: repository("alice"),
        pull_request: provider_coordinates("PR_42"),
        pull_request_url: "https://github.com/acme/rocket/pull/42".into(),
        pull_request_state: "OPEN".into(),
        current_base_sha: base.into(),
        current_head_sha: head.into(),
    }
}

#[test]
fn canonical_file_target_supports_binary_rename_but_rejects_raw_or_changed_identity() {
    let mut changed = file("assets/new.bin");
    changed.previous_path = Some("assets/old.bin".into());
    changed.status = "renamed".into();
    changed.patch = None;
    changed.patch_complete = false;
    let canonical = comparison("base", "head", changed.clone());
    let displayed = ReviewSession::new(canonical.clone());
    let target =
        map_file_to_canonical_published(&displayed, &file_key(&changed), published(&canonical))
            .unwrap();
    assert_eq!(target.path, "assets/new.bin");
    assert_eq!(target.previous_path.as_deref(), Some("assets/old.bin"));
    assert_eq!(target.base_sha, "base");
    assert_eq!(target.commit_sha, "head");

    let mut raw = changed.clone();
    raw.raw_path = Some(vec![0xff]);
    let raw_comparison = comparison("base", "head", raw.clone());
    let raw_session = ReviewSession::new(raw_comparison.clone());
    assert!(matches!(
        map_file_to_canonical_published(&raw_session, &file_key(&raw), published(&raw_comparison)),
        Err(MappingIssue::RawPathUnsupported)
    ));

    let mut different = changed;
    different.previous_path = Some("assets/another.bin".into());
    let different_comparison = comparison("base", "head", different);
    assert!(matches!(
        map_file_to_canonical_published(
            &displayed,
            &target.file_key,
            published(&different_comparison)
        ),
        Err(MappingIssue::FileIdentityChanged)
    ));
}

#[test]
fn file_draft_ack_updates_only_exact_frozen_target_and_body() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let session = ReviewSession::new(canonical.clone());
    let target =
        map_file_to_canonical_published(&session, "src/lib.rs", published(&canonical)).unwrap();
    let mut other_target = target.clone();
    other_target.path = "src/other.rs".into();
    other_target.file_key = "src/other.rs".into();
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let first = state
        .add_file_draft(target.clone(), "sent exact file body")
        .unwrap()
        .id
        .clone();
    let other = state
        .add_file_draft(other_target, "unrelated file body")
        .unwrap()
        .id
        .clone();
    let intent = state
        .prepare_pending_file_comment(&first, &pending_file_source("base", "head"))
        .unwrap();
    assert!(matches!(intent.target, ReviewCommentTarget::File(_)));
    state
        .mark_in_flight(&intent.operation_id, "attempt-1")
        .unwrap();
    state.edit_file_draft(&other, "unrelated edit").unwrap();
    state
        .reconcile_observed_file_comment_success(
            &intent.operation_id,
            "REVIEW_pending".into(),
            "COMMENT_new".into(),
        )
        .unwrap();
    assert_eq!(
        state
            .file_draft(&first)
            .unwrap()
            .remote
            .as_ref()
            .map(|remote| remote.comment_id.as_str()),
        Some("COMMENT_new")
    );
    assert_eq!(state.file_draft(&other).unwrap().body, "unrelated edit");
    assert!(state.file_draft(&other).unwrap().remote.is_none());

    let changed = state
        .add_file_draft(target, "original frozen body")
        .unwrap()
        .id
        .clone();
    let changed_intent = state
        .prepare_pending_file_comment(&changed, &pending_file_source("base", "head"))
        .unwrap();
    state
        .mark_in_flight(&changed_intent.operation_id, "attempt-2")
        .unwrap();
    state.edit_file_draft(&changed, "new unsent edit").unwrap();
    state
        .reconcile_observed_file_comment_success(
            &changed_intent.operation_id,
            "REVIEW_pending".into(),
            "COMMENT_old_payload".into(),
        )
        .unwrap();
    assert_eq!(state.file_draft(&changed).unwrap().body, "new unsent edit");
    assert!(state.file_draft(&changed).unwrap().remote.is_none());
}

#[test]
fn pending_file_freshness_witness_never_survives_snapshot_serialization() {
    let snapshot = PendingReviewSnapshot {
        review: PullRequestReview {
            coordinates: provider_coordinates("REVIEW_pending"),
            author: Some("alice".into()),
            body: String::new(),
            state: "PENDING".into(),
            submitted_at: None,
            commit_sha: Some("head".into()),
            edit_summary_capability: None,
            dismissal_capability: None,
            url: String::new(),
        },
        comments: Vec::new(),
        comments_complete: true,
        file_comment_source: Some(pending_file_source("base", "head")),
    };
    let encoded = serde_json::to_vec(&snapshot).unwrap();
    assert!(!String::from_utf8_lossy(&encoded).contains("file_comment_source"));
    let decoded: PendingReviewSnapshot = serde_json::from_slice(&encoded).unwrap();
    assert!(decoded.file_comment_source.is_none());

    let old_thread = serde_json::json!({
        "coordinates": provider_coordinates("THREAD_old"),
        "path": "src/lib.rs",
        "line": null,
        "original_line": null,
        "start_line": null,
        "original_start_line": null,
        "side": null,
        "start_side": null,
        "resolved": false,
        "outdated": false,
        "comments": [],
        "comments_complete": true
    });
    let decoded_thread: ReviewThread = serde_json::from_value(old_thread).unwrap();
    assert_eq!(decoded_thread.subject, ReviewSubject::Unknown);
}

#[test]
fn pending_review_start_freezes_distinct_stages_and_clears_only_exact_predecessor() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let session = ReviewSession::new(canonical.clone());
    let target =
        map_file_to_canonical_published(&session, "src/lib.rs", published(&canonical)).unwrap();
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let draft_id = state
        .add_file_draft(target.clone(), "frozen whole-file body")
        .unwrap()
        .id
        .clone();
    let unrelated_id = state
        .add_file_draft(
            cibergit::participation::PublishedFile {
                path: "src/other.rs".into(),
                file_key: "src/other.rs".into(),
                ..target.clone()
            },
            "unrelated",
        )
        .unwrap()
        .id
        .clone();
    let intent = state
        .prepare_pending_file_review_start(
            &draft_id,
            &pending_file_absence("base", "head"),
            "flow-1".into(),
            "create-operation-1".into(),
            "thread-operation-1".into(),
        )
        .unwrap();
    assert_ne!(intent.create_operation_id, intent.thread_operation_id);
    assert!(matches!(intent.target, ReviewCommentTarget::File(_)));

    state
        .edit_file_draft(&draft_id, "newer unsent body")
        .unwrap();
    state
        .edit_file_draft(&unrelated_id, "unrelated newer body")
        .unwrap();
    state
        .reconcile_pending_file_review_start_success(
            &intent,
            "REVIEW_created".into(),
            "COMMENT_created".into(),
        )
        .unwrap();

    assert_eq!(
        state.file_draft(&draft_id).unwrap().body,
        "newer unsent body"
    );
    assert!(state.file_draft(&draft_id).unwrap().remote.is_none());
    assert_eq!(
        state.file_draft(&unrelated_id).unwrap().body,
        "unrelated newer body"
    );
    assert!(state.file_draft(&unrelated_id).unwrap().remote.is_none());
    assert_eq!(
        state.acknowledged_pending_review_id.as_deref(),
        Some("REVIEW_created")
    );
}

#[test]
fn pending_review_start_rejects_absence_identity_and_duplicate_stage_ids() {
    let canonical = comparison("base", "head", file("src/lib.rs"));
    let session = ReviewSession::new(canonical.clone());
    let target =
        map_file_to_canonical_published(&session, "src/lib.rs", published(&canonical)).unwrap();
    let mut state = ReviewComposition::new(key("alice"), revision("base", "head")).unwrap();
    let draft_id = state
        .add_file_draft(target, "whole-file body")
        .unwrap()
        .id
        .clone();
    let mut wrong = pending_file_absence("base", "head");
    wrong.viewer_login = "mallory".into();
    assert!(
        state
            .prepare_pending_file_review_start(
                &draft_id,
                &wrong,
                "flow-1".into(),
                "create-1".into(),
                "thread-1".into(),
            )
            .is_err()
    );
    let mut wrong_url = pending_file_absence("base", "head");
    wrong_url.pull_request_url = "https://github.com/acme/rocket/pull/43".into();
    assert!(
        state
            .prepare_pending_file_review_start(
                &draft_id,
                &wrong_url,
                "flow-url".into(),
                "create-url".into(),
                "thread-url".into(),
            )
            .is_err()
    );
    assert!(
        state
            .prepare_pending_file_review_start(
                &draft_id,
                &pending_file_absence("base", "head"),
                "flow-1".into(),
                "same".into(),
                "same".into(),
            )
            .is_err()
    );
}
