//! Durable state for creating an empty pending review and then adding one
//! whole-file comment to the exact returned review ID.
//!
//! This module owns persistence and transition validation only. Provider
//! transport is never replayed while loading a record.

use crate::{
    domain::PendingReviewCreationAcknowledgement,
    participation::{PendingFileReviewStartIntent, validate_pending_file_review_start_intent},
};
use serde::{Deserialize, Serialize};

pub const MAX_PENDING_REVIEW_START_BYTES: usize = 512 * 1024;
pub const MAX_PENDING_REVIEW_START_ID_BYTES: usize = 1024;
pub const MAX_PENDING_REVIEW_START_REASON_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingReviewStartWriteStage {
    CreateReview,
    AddFileThread,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingFileThreadReceipt {
    pub operation_id: String,
    pub review_id: String,
    pub thread_id: String,
    pub comment_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingReviewStartStage {
    PreparedCreate,
    /// A crash can leave PreparedCreate durable before any provider dispatch.
    /// This explicit local disposition releases the target without claiming a
    /// remote write and without authorizing a replay.
    CancelledBeforeCreate {
        reason: String,
    },
    CreateInFlight {
        attempt_id: String,
    },
    ReviewCreated {
        creation: PendingReviewCreationAcknowledgement,
        #[serde(default)]
        stop_reason: Option<String>,
    },
    ThreadInFlight {
        attempt_id: String,
        creation: PendingReviewCreationAcknowledgement,
    },
    /// The exact remote FILE IDs are durable. No transport may run again;
    /// only exact local predecessor reconciliation may remain.
    ThreadAcknowledged {
        creation: PendingReviewCreationAcknowledgement,
        thread: PendingFileThreadReceipt,
    },
    ThreadNotApplied {
        creation: PendingReviewCreationAcknowledgement,
        attempt_id: String,
        evidence: String,
    },
    Acknowledged {
        creation: PendingReviewCreationAcknowledgement,
        thread: PendingFileThreadReceipt,
    },
    /// Explicit local disposition after stage 1. The pending review remains on
    /// GitHub, the FILE draft remains local, and no continuation is authorized.
    StoppedAfterReviewCreated {
        creation: PendingReviewCreationAcknowledgement,
        reason: String,
    },
    CreateNotApplied {
        attempt_id: String,
        evidence: String,
    },
    Uncertain {
        stage: PendingReviewStartWriteStage,
        attempt_id: String,
        creation: Option<PendingReviewCreationAcknowledgement>,
        thread: Option<PendingFileThreadReceipt>,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReviewStartRecord {
    pub intent: PendingFileReviewStartIntent,
    pub stage: PendingReviewStartStage,
}

impl PendingReviewStartRecord {
    pub fn new(intent: PendingFileReviewStartIntent) -> Result<Self, String> {
        validate_pending_file_review_start_intent(&intent).map_err(|error| error.to_string())?;
        let record = Self {
            intent,
            stage: PendingReviewStartStage::PreparedCreate,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn mark_create_in_flight(&mut self, attempt_id: String) -> Result<(), String> {
        validate_id("create attempt", &attempt_id)?;
        if !matches!(&self.stage, PendingReviewStartStage::PreparedCreate) {
            return Err("pending-review creation is not in the prepared stage".into());
        }
        self.stage = PendingReviewStartStage::CreateInFlight { attempt_id };
        self.validate()
    }

    pub fn cancel_before_create(&mut self, reason: String) -> Result<(), String> {
        validate_reason("pre-create cancellation", &reason)?;
        if !matches!(&self.stage, PendingReviewStartStage::PreparedCreate) {
            return Err("only a prepared start with no create dispatch can be cancelled".into());
        }
        self.stage = PendingReviewStartStage::CancelledBeforeCreate { reason };
        self.validate()
    }

    pub fn mark_create_not_applied(
        &mut self,
        attempt_id: String,
        evidence: String,
    ) -> Result<(), String> {
        validate_id("create attempt", &attempt_id)?;
        validate_reason("create rejection", &evidence)?;
        if !matches!(
            &self.stage,
            PendingReviewStartStage::CreateInFlight { attempt_id: current }
                if current == &attempt_id
        ) && !matches!(&self.stage, PendingReviewStartStage::PreparedCreate)
        {
            return Err("only the exact prepared create attempt can record NotApplied".into());
        }
        self.stage = PendingReviewStartStage::CreateNotApplied {
            attempt_id,
            evidence,
        };
        self.validate()
    }

    pub fn mark_review_created(
        &mut self,
        creation: PendingReviewCreationAcknowledgement,
    ) -> Result<(), String> {
        if !matches!(&self.stage, PendingReviewStartStage::CreateInFlight { .. }) {
            return Err("only an in-flight create stage can record a created review".into());
        }
        self.stage = PendingReviewStartStage::ReviewCreated {
            creation,
            stop_reason: None,
        };
        self.validate()
    }

    pub fn stop_after_review_created(&mut self, reason: String) -> Result<(), String> {
        validate_reason("created-review stop reason", &reason)?;
        let PendingReviewStartStage::ReviewCreated {
            creation,
            stop_reason: _,
        } = &self.stage
        else {
            return Err("only a durably created review can stop before the FILE write".into());
        };
        self.stage = PendingReviewStartStage::ReviewCreated {
            creation: creation.clone(),
            stop_reason: Some(reason),
        };
        self.validate()
    }

    pub fn mark_thread_in_flight(&mut self, attempt_id: String) -> Result<(), String> {
        validate_id("thread attempt", &attempt_id)?;
        let PendingReviewStartStage::ReviewCreated { creation, .. } = &self.stage else {
            return Err("FILE dispatch requires a durably created exact review".into());
        };
        self.stage = PendingReviewStartStage::ThreadInFlight {
            attempt_id,
            creation: creation.clone(),
        };
        self.validate()
    }

    pub fn mark_thread_acknowledged(
        &mut self,
        thread: PendingFileThreadReceipt,
    ) -> Result<(), String> {
        let PendingReviewStartStage::ThreadInFlight { creation, .. } = &self.stage else {
            return Err("only an in-flight FILE stage can record remote IDs".into());
        };
        self.stage = PendingReviewStartStage::ThreadAcknowledged {
            creation: creation.clone(),
            thread,
        };
        self.validate()
    }

    pub fn mark_thread_not_applied(&mut self, evidence: String) -> Result<(), String> {
        validate_reason("FILE rejection", &evidence)?;
        let PendingReviewStartStage::ThreadInFlight {
            attempt_id,
            creation,
        } = &self.stage
        else {
            return Err("only an in-flight FILE stage can record NotApplied".into());
        };
        self.stage = PendingReviewStartStage::ThreadNotApplied {
            creation: creation.clone(),
            attempt_id: attempt_id.clone(),
            evidence,
        };
        self.validate()
    }

    pub fn mark_acknowledged(&mut self) -> Result<(), String> {
        let PendingReviewStartStage::ThreadAcknowledged { creation, thread } = &self.stage else {
            return Err("local completion requires durable exact FILE acknowledgement IDs".into());
        };
        self.stage = PendingReviewStartStage::Acknowledged {
            creation: creation.clone(),
            thread: thread.clone(),
        };
        self.validate()
    }

    pub fn stop_and_keep_created_review(&mut self, reason: String) -> Result<(), String> {
        validate_reason("created-review stop disposition", &reason)?;
        let PendingReviewStartStage::ReviewCreated { creation, .. } = &self.stage else {
            return Err(
                "only a quiescent created review with no FILE dispatch can be stopped".into(),
            );
        };
        self.stage = PendingReviewStartStage::StoppedAfterReviewCreated {
            creation: creation.clone(),
            reason,
        };
        self.validate()
    }

    pub fn mark_uncertain(&mut self, reason: String) -> Result<(), String> {
        validate_reason("uncertain outcome", &reason)?;
        let (stage, attempt_id, creation, thread) = match &self.stage {
            PendingReviewStartStage::CreateInFlight { attempt_id } => (
                PendingReviewStartWriteStage::CreateReview,
                attempt_id.clone(),
                None,
                None,
            ),
            PendingReviewStartStage::ThreadInFlight {
                attempt_id,
                creation,
            } => (
                PendingReviewStartWriteStage::AddFileThread,
                attempt_id.clone(),
                Some(creation.clone()),
                None,
            ),
            PendingReviewStartStage::ThreadAcknowledged { creation, thread } => (
                PendingReviewStartWriteStage::AddFileThread,
                self.intent.thread_operation_id.clone(),
                Some(creation.clone()),
                Some(thread.clone()),
            ),
            _ => {
                return Err(
                    "only an active or locally incomplete provider stage can become uncertain"
                        .into(),
                );
            }
        };
        self.stage = PendingReviewStartStage::Uncertain {
            stage,
            attempt_id,
            creation,
            thread,
            reason,
        };
        self.validate()
    }

    /// Freeze a stage when a zero-transport terminal disposition could not be
    /// saved. This is never a transport replay signal.
    pub fn mark_terminal_persistence_uncertain(
        &mut self,
        stage: PendingReviewStartWriteStage,
        attempt_id: String,
        reason: String,
    ) -> Result<(), String> {
        validate_id("uncertain attempt", &attempt_id)?;
        validate_reason("uncertain outcome", &reason)?;
        let creation = self.creation().cloned();
        if matches!(stage, PendingReviewStartWriteStage::AddFileThread) && creation.is_none() {
            return Err("uncertain FILE persistence must retain the exact created review".into());
        }
        self.stage = PendingReviewStartStage::Uncertain {
            stage,
            attempt_id,
            creation,
            thread: None,
            reason,
        };
        self.validate()
    }

    pub fn creation(&self) -> Option<&PendingReviewCreationAcknowledgement> {
        match &self.stage {
            PendingReviewStartStage::ReviewCreated { creation, .. }
            | PendingReviewStartStage::ThreadInFlight { creation, .. }
            | PendingReviewStartStage::ThreadAcknowledged { creation, .. }
            | PendingReviewStartStage::Acknowledged { creation, .. }
            | PendingReviewStartStage::ThreadNotApplied { creation, .. }
            | PendingReviewStartStage::StoppedAfterReviewCreated { creation, .. } => Some(creation),
            PendingReviewStartStage::Uncertain { creation, .. } => creation.as_ref(),
            _ => None,
        }
    }

    pub fn may_continue_file_thread(&self) -> bool {
        matches!(&self.stage, PendingReviewStartStage::ReviewCreated { .. })
    }

    pub fn blocks_target_mutations(&self) -> bool {
        matches!(
            &self.stage,
            PendingReviewStartStage::PreparedCreate
                | PendingReviewStartStage::CreateInFlight { .. }
                | PendingReviewStartStage::ReviewCreated { .. }
                | PendingReviewStartStage::ThreadInFlight { .. }
                | PendingReviewStartStage::ThreadAcknowledged { .. }
                | PendingReviewStartStage::Uncertain { .. }
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_pending_file_review_start_intent(&self.intent)
            .map_err(|error| error.to_string())?;
        match &self.stage {
            PendingReviewStartStage::PreparedCreate => {}
            PendingReviewStartStage::CancelledBeforeCreate { reason } => {
                validate_reason("pre-create cancellation", reason)?;
            }
            PendingReviewStartStage::CreateInFlight { attempt_id } => {
                validate_id("create attempt", attempt_id)?;
            }
            PendingReviewStartStage::ReviewCreated {
                creation,
                stop_reason,
            } => {
                validate_creation(&self.intent, creation)?;
                if let Some(reason) = stop_reason {
                    validate_reason("created-review stop reason", reason)?;
                }
            }
            PendingReviewStartStage::ThreadInFlight {
                attempt_id,
                creation,
            } => {
                validate_id("thread attempt", attempt_id)?;
                validate_creation(&self.intent, creation)?;
            }
            PendingReviewStartStage::ThreadAcknowledged { creation, thread }
            | PendingReviewStartStage::Acknowledged { creation, thread } => {
                validate_creation(&self.intent, creation)?;
                validate_thread(&self.intent, creation, thread)?;
            }
            PendingReviewStartStage::ThreadNotApplied {
                creation,
                attempt_id,
                evidence,
            } => {
                validate_creation(&self.intent, creation)?;
                validate_id("thread attempt", attempt_id)?;
                validate_reason("FILE rejection", evidence)?;
            }
            PendingReviewStartStage::StoppedAfterReviewCreated { creation, reason } => {
                validate_creation(&self.intent, creation)?;
                validate_reason("created-review stop disposition", reason)?;
            }
            PendingReviewStartStage::CreateNotApplied {
                attempt_id,
                evidence,
            } => {
                validate_id("create attempt", attempt_id)?;
                validate_reason("create rejection", evidence)?;
            }
            PendingReviewStartStage::Uncertain {
                stage,
                attempt_id,
                creation,
                thread,
                reason,
            } => {
                validate_id("uncertain attempt", attempt_id)?;
                validate_reason("uncertain outcome", reason)?;
                match stage {
                    PendingReviewStartWriteStage::CreateReview => {
                        if creation.is_some() || thread.is_some() {
                            return Err("uncertain create cannot claim unknown remote IDs".into());
                        }
                    }
                    PendingReviewStartWriteStage::AddFileThread => {
                        let creation = creation.as_ref().ok_or_else(|| {
                            "uncertain FILE stage must retain the exact created review".to_owned()
                        })?;
                        validate_creation(&self.intent, creation)?;
                        if let Some(thread) = thread {
                            validate_thread(&self.intent, creation, thread)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn validate_creation(
    intent: &PendingFileReviewStartIntent,
    creation: &PendingReviewCreationAcknowledgement,
) -> Result<(), String> {
    for (kind, value) in [
        ("create operation", creation.operation_id.as_str()),
        ("created review", creation.review.remote_id.as_str()),
        ("created review author", creation.review_author.as_str()),
        ("created review commit", creation.review_commit_sha.as_str()),
        (
            "created review pull request",
            creation.pull_request.remote_id.as_str(),
        ),
        (
            "created review repository",
            creation.repository_name_with_owner.as_str(),
        ),
    ] {
        validate_id(kind, value)?;
    }
    if creation.operation_id != intent.create_operation_id
        || !intent.key.matches(&creation.review)
        || creation.pull_request != intent.pull_request
        || creation.review.remote_id == creation.pull_request.remote_id
        || !creation
            .review_author
            .eq_ignore_ascii_case(&intent.selected_author)
        || creation.review_commit_sha != intent.observed_head_sha
        || !creation
            .repository_name_with_owner
            .eq_ignore_ascii_case(&format!("{}/{}", intent.key.owner, intent.key.repository))
    {
        return Err("Created review receipt does not match the frozen exact target.".into());
    }
    Ok(())
}

fn validate_thread(
    intent: &PendingFileReviewStartIntent,
    creation: &PendingReviewCreationAcknowledgement,
    thread: &PendingFileThreadReceipt,
) -> Result<(), String> {
    for (kind, value) in [
        ("thread operation", thread.operation_id.as_str()),
        ("thread review", thread.review_id.as_str()),
        ("created file thread", thread.thread_id.as_str()),
        ("created file comment", thread.comment_id.as_str()),
    ] {
        validate_id(kind, value)?;
    }
    if thread.operation_id != intent.thread_operation_id
        || thread.review_id != creation.review.remote_id
        || thread.review_id == thread.thread_id
        || thread.review_id == thread.comment_id
        || thread.thread_id == thread.comment_id
    {
        return Err("FILE receipt does not match the frozen exact created review.".into());
    }
    Ok(())
}

fn validate_id(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_PENDING_REVIEW_START_ID_BYTES || value.contains('\0') {
        Err(format!("{kind} identity is empty, oversized, or invalid"))
    } else {
        Ok(())
    }
}

fn validate_reason(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_PENDING_REVIEW_START_REASON_BYTES
        || value.contains('\0')
    {
        Err(format!("{kind} is empty, oversized, or invalid"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_target_is_part_of_the_durable_intent_shape() {
        let source = include_str!("participation.rs");
        assert!(source.contains("pub target: ReviewCommentTarget"));
        assert!(matches!(
            crate::participation::ReviewCommentTarget::File(crate::participation::PublishedFile {
                base_sha: "base".into(),
                commit_sha: "head".into(),
                path: "src/lib.rs".into(),
                previous_path: None,
                raw_previous_path: None,
                file_key: "src/lib.rs".into(),
            }),
            crate::participation::ReviewCommentTarget::File(_)
        ));
    }
}
