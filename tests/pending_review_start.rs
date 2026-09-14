use cibergit::{
    domain::{Account, PendingReviewCreationAcknowledgement, ProviderCoordinates},
    participation::{PendingFileReviewStartIntent, PublishedFile, ReviewCommentTarget, ReviewKey},
    pending_review_start::{
        PendingFileThreadReceipt, PendingReviewStartRecord, PendingReviewStartStage,
        PendingReviewStartWriteStage,
    },
};

fn key() -> ReviewKey {
    ReviewKey {
        provider: "github".into(),
        host: "github.com".into(),
        owner: "acme".into(),
        repository: "rocket".into(),
        account: Account {
            host: "github.com".into(),
            login: "alice".into(),
        },
        pull_request: 42,
    }
}

fn coordinates(remote_id: &str) -> ProviderCoordinates {
    ProviderCoordinates {
        provider: "github".into(),
        host: "github.com".into(),
        owner: "acme".into(),
        repository: "rocket".into(),
        pull_request: 42,
        remote_id: remote_id.into(),
    }
}

fn intent(body: String) -> PendingFileReviewStartIntent {
    PendingFileReviewStartIntent {
        flow_id: "flow-1".into(),
        create_operation_id: "create-operation-1".into(),
        thread_operation_id: "thread-operation-1".into(),
        key: key(),
        draft_id: "draft-1".into(),
        body,
        target: ReviewCommentTarget::File(PublishedFile {
            base_sha: "base".into(),
            commit_sha: "head".into(),
            file_key: "src/lib.rs".into(),
            path: "src/lib.rs".into(),
            previous_path: None,
            raw_previous_path: None,
        }),
        pull_request: coordinates("PR_42"),
        pull_request_url: "https://github.com/acme/rocket/pull/42".into(),
        selected_author: "alice".into(),
        observed_base_sha: "base".into(),
        observed_head_sha: "head".into(),
    }
}

fn creation() -> PendingReviewCreationAcknowledgement {
    PendingReviewCreationAcknowledgement {
        operation_id: "create-operation-1".into(),
        review: coordinates("REVIEW_created"),
        review_author: "alice".into(),
        review_commit_sha: "head".into(),
        pull_request: coordinates("PR_42"),
        repository_name_with_owner: "acme/rocket".into(),
    }
}

#[test]
fn durable_id_is_required_before_file_stage_and_transitions_do_not_replay() {
    let mut record = PendingReviewStartRecord::new(intent("exact body".into())).unwrap();
    assert!(
        record
            .mark_thread_in_flight("thread-attempt".into())
            .is_err()
    );
    record
        .mark_create_in_flight("create-attempt".into())
        .unwrap();
    assert!(
        record
            .mark_thread_in_flight("thread-attempt".into())
            .is_err()
    );
    record.mark_review_created(creation()).unwrap();
    assert!(record.may_continue_file_thread());
    record
        .mark_thread_in_flight("thread-attempt".into())
        .unwrap();
    assert!(!record.may_continue_file_thread());
    record
        .mark_uncertain("lost exact FILE acknowledgement".into())
        .unwrap();
    assert!(matches!(
        record.stage,
        PendingReviewStartStage::Uncertain {
            stage: PendingReviewStartWriteStage::AddFileThread,
            ..
        }
    ));
    assert!(record.mark_thread_in_flight("replay".into()).is_err());
}

#[test]
fn explicit_stop_is_only_available_before_file_dispatch_and_keeps_created_id() {
    let mut record = PendingReviewStartRecord::new(intent("exact body".into())).unwrap();
    assert!(
        record
            .stop_and_keep_created_review("user stopped".into())
            .is_err()
    );
    record
        .mark_create_in_flight("create-attempt".into())
        .unwrap();
    record.mark_review_created(creation()).unwrap();
    record
        .stop_and_keep_created_review("user stopped; review kept".into())
        .unwrap();
    assert!(matches!(
        record.stage,
        PendingReviewStartStage::StoppedAfterReviewCreated { .. }
    ));
    assert_eq!(
        record.creation().unwrap().review.remote_id,
        "REVIEW_created"
    );
    assert!(!record.blocks_target_mutations());
    assert!(!record.may_continue_file_thread());
    assert!(record.mark_thread_in_flight("late".into()).is_err());
}

#[test]
fn proven_zero_transport_file_rejection_remains_blocking_until_continue_or_stop() {
    let mut record = PendingReviewStartRecord::new(intent("exact body".into())).unwrap();
    record
        .mark_create_in_flight("create-attempt".into())
        .unwrap();
    record.mark_review_created(creation()).unwrap();
    record
        .mark_thread_in_flight("thread-attempt".into())
        .unwrap();
    record
        .mark_thread_not_applied("credential lookup failed before transport".into())
        .unwrap();
    assert!(record.blocks_target_mutations());
    assert!(record.may_continue_file_thread());
    assert_eq!(
        record.creation().unwrap().review.remote_id,
        "REVIEW_created"
    );

    let mut continued = record.clone();
    continued
        .mark_thread_in_flight("new-confirmed-attempt".into())
        .unwrap();
    assert!(matches!(
        continued.stage,
        PendingReviewStartStage::ThreadInFlight { .. }
    ));

    record
        .stop_and_keep_created_review("user kept review and revoked continuation".into())
        .unwrap();
    assert!(matches!(
        record.stage,
        PendingReviewStartStage::StoppedAfterReviewCreated { .. }
    ));
    assert!(!record.blocks_target_mutations());
    assert!(!record.may_continue_file_thread());
}

#[test]
fn recovered_prepared_start_can_only_be_cancelled_as_zero_transport() {
    let mut record = PendingReviewStartRecord::new(intent("exact body".into())).unwrap();
    record
        .cancel_before_create("user cancelled recovered preparation".into())
        .unwrap();
    assert!(matches!(
        record.stage,
        PendingReviewStartStage::CancelledBeforeCreate { .. }
    ));
    assert!(!record.blocks_target_mutations());
    assert!(record.mark_create_in_flight("late-create".into()).is_err());
    assert!(record.creation().is_none());
}

#[test]
fn compact_receipts_reject_wrong_review_and_duplicate_ids() {
    let mut record = PendingReviewStartRecord::new(intent("exact body".into())).unwrap();
    record
        .mark_create_in_flight("create-attempt".into())
        .unwrap();
    record.mark_review_created(creation()).unwrap();
    record
        .mark_thread_in_flight("thread-attempt".into())
        .unwrap();
    assert!(
        record
            .mark_thread_acknowledged(PendingFileThreadReceipt {
                operation_id: "thread-operation-1".into(),
                review_id: "REVIEW_other".into(),
                thread_id: "THREAD_new".into(),
                comment_id: "COMMENT_new".into(),
            })
            .is_err()
    );

    let mut record = PendingReviewStartRecord::new(intent("exact body".into())).unwrap();
    record
        .mark_create_in_flight("create-attempt".into())
        .unwrap();
    record.mark_review_created(creation()).unwrap();
    record
        .mark_thread_in_flight("thread-attempt".into())
        .unwrap();
    assert!(
        record
            .mark_thread_acknowledged(PendingFileThreadReceipt {
                operation_id: "thread-operation-1".into(),
                review_id: "REVIEW_created".into(),
                thread_id: "COMMENT_same".into(),
                comment_id: "COMMENT_same".into(),
            })
            .is_err()
    );
}
