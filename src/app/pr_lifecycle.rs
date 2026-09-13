//! Native PR lifecycle controller state. Provider I/O stays in `app.rs`; this
//! module owns frozen target/action witnesses and never mutates ReviewSession.

use cibergit::domain::{
    IssueComment, ProviderChoice, PullRequestDiscussionAction, PullRequestDiscussionRequest,
    PullRequestLifecycleAction, PullRequestLifecycleChoices, PullRequestLifecycleRequest,
    PullRequestLifecycleSnapshot, PullRequestMutationTarget, PullRequestReviewer, Repository,
};

const MAX_FORM_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataForm {
    pub title: String,
    pub body: String,
    pub base_branch: String,
}

impl MetadataForm {
    fn from_snapshot(snapshot: &PullRequestLifecycleSnapshot) -> Self {
        Self {
            title: snapshot.title.clone(),
            body: snapshot.body.clone(),
            base_branch: snapshot.base_branch.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscussionMode {
    Create,
    Edit {
        comment: Box<IssueComment>,
        selected_author: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionForm {
    pub mode: DiscussionMode,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrozenMutation {
    Lifecycle {
        request: PullRequestLifecycleRequest,
        form_witness: Option<MetadataForm>,
        summary: String,
    },
    Discussion {
        request: PullRequestDiscussionRequest,
        body_witness: Option<String>,
        summary: String,
    },
}

impl FrozenMutation {
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Lifecycle { request, .. } => &request.operation_id,
            Self::Discussion { request, .. } => &request.operation_id,
        }
    }

    pub fn summary(&self) -> &str {
        match self {
            Self::Lifecycle { summary, .. } | Self::Discussion { summary, .. } => summary,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrLifecycleController {
    repository: Repository,
    pull_request: u64,
    pub snapshot: Option<PullRequestLifecycleSnapshot>,
    pub choices: Option<PullRequestLifecycleChoices>,
    pub metadata_form: Option<MetadataForm>,
    pub editing_metadata: bool,
    pub discussion_form: Option<DiscussionForm>,
    pub confirmation: Option<FrozenMutation>,
    pub active_operation: Option<String>,
    pub notice: Option<String>,
}

impl PrLifecycleController {
    pub fn new(repository: Repository, pull_request: u64) -> Self {
        Self {
            repository,
            pull_request,
            snapshot: None,
            choices: None,
            metadata_form: None,
            editing_metadata: false,
            discussion_form: None,
            confirmation: None,
            active_operation: None,
            notice: None,
        }
    }

    pub fn install_snapshot(
        &mut self,
        snapshot: PullRequestLifecycleSnapshot,
    ) -> Result<(), String> {
        self.validate_snapshot(&snapshot)?;
        if !self.editing_metadata {
            self.metadata_form = Some(MetadataForm::from_snapshot(&snapshot));
        }
        self.snapshot = Some(snapshot);
        Ok(())
    }

    pub fn install_choices(&mut self, choices: PullRequestLifecycleChoices) -> Result<(), String> {
        if choices.repository.cache_key() != self.repository.cache_key() {
            return Err("Lifecycle choices target another account or repository.".into());
        }
        self.choices = Some(choices);
        Ok(())
    }

    pub fn begin_metadata_edit(&mut self) -> Result<MetadataForm, String> {
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| "Lifecycle metadata is still loading.".to_owned())?;
        if !snapshot.can_update_metadata.available {
            return Err(capability_reason(
                "Metadata editing is unavailable",
                &snapshot.can_update_metadata.reason,
            ));
        }
        let form = MetadataForm::from_snapshot(snapshot);
        self.metadata_form = Some(form.clone());
        self.editing_metadata = true;
        self.confirmation = None;
        Ok(form)
    }

    pub fn stage_metadata(&mut self, title: String, body: String, base_branch: String) {
        if self.editing_metadata {
            self.metadata_form = Some(MetadataForm {
                title,
                body,
                base_branch,
            });
        }
    }

    pub fn cancel_metadata(&mut self) {
        self.editing_metadata = false;
        self.confirmation = None;
        self.metadata_form = self.snapshot.as_ref().map(MetadataForm::from_snapshot);
    }

    pub fn prepare_metadata_apply(
        &mut self,
        operation_id: String,
        attempt_id: String,
    ) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        let form = self
            .metadata_form
            .clone()
            .ok_or_else(|| "Metadata form is unavailable.".to_owned())?;
        validate_form(&form)?;
        let mut actions = Vec::new();
        if form.title != snapshot.title {
            actions.push((
                PullRequestLifecycleAction::UpdateTitle {
                    observed: snapshot.title.clone(),
                    value: form.title.clone(),
                },
                format!("Title: {:?} → {:?}", snapshot.title, form.title),
            ));
        }
        if form.body != snapshot.body {
            actions.push((
                PullRequestLifecycleAction::UpdateBody {
                    observed: snapshot.body.clone(),
                    value: form.body.clone(),
                },
                format!("Body: {:?} → {:?}", snapshot.body, form.body),
            ));
        }
        if form.base_branch != snapshot.base_branch {
            actions.push((
                PullRequestLifecycleAction::UpdateBaseBranch {
                    observed: snapshot.base_branch.clone(),
                    value: form.base_branch.clone(),
                },
                format!(
                    "Base branch: {:?} → {:?}",
                    snapshot.base_branch, form.base_branch
                ),
            ));
        }
        let [(action, summary)] = actions.as_slice() else {
            return Err(if actions.is_empty() {
                "No metadata value changed.".into()
            } else {
                "Apply one metadata field at a time so each confirmation and provider acknowledgement has one exact affected value."
                    .into()
            });
        };
        self.confirmation = Some(FrozenMutation::Lifecycle {
            request: PullRequestLifecycleRequest {
                operation_id,
                attempt_id,
                target: mutation_target(snapshot),
                action: action.clone(),
            },
            form_witness: Some(form),
            summary: summary.clone(),
        });
        Ok(())
    }

    pub fn prepare_lifecycle_action(
        &mut self,
        action: PullRequestLifecycleAction,
        operation_id: String,
        attempt_id: String,
    ) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        validate_action_capability(snapshot, &action)?;
        let summary = lifecycle_summary(&action);
        self.confirmation = Some(FrozenMutation::Lifecycle {
            request: PullRequestLifecycleRequest {
                operation_id,
                attempt_id,
                target: mutation_target(snapshot),
                action,
            },
            form_witness: None,
            summary,
        });
        Ok(())
    }

    pub fn begin_comment_create(&mut self) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        if !snapshot.can_comment.available {
            return Err(capability_reason(
                "Commenting is unavailable",
                &snapshot.can_comment.reason,
            ));
        }
        self.discussion_form = Some(DiscussionForm {
            mode: DiscussionMode::Create,
            body: String::new(),
        });
        self.confirmation = None;
        Ok(())
    }

    pub fn begin_comment_edit(&mut self, comment: IssueComment) -> Result<String, String> {
        let snapshot = self.snapshot()?;
        let author = comment
            .author
            .clone()
            .ok_or_else(|| "A comment without an exact author cannot be edited.".to_owned())?;
        if !author.eq_ignore_ascii_case(&snapshot.viewer_login) {
            return Err("Only an exact current-user issue comment can be edited.".into());
        }
        validate_comment_target(&self.repository, self.pull_request, &comment)?;
        self.discussion_form = Some(DiscussionForm {
            mode: DiscussionMode::Edit {
                comment: Box::new(comment.clone()),
                selected_author: author,
            },
            body: comment.body.clone(),
        });
        self.confirmation = None;
        Ok(comment.body)
    }

    pub fn stage_discussion_body(&mut self, body: String) {
        if let Some(form) = &mut self.discussion_form {
            form.body = body;
        }
    }

    pub fn cancel_discussion(&mut self) {
        self.discussion_form = None;
        self.confirmation = None;
    }

    pub fn prepare_discussion_apply(
        &mut self,
        operation_id: String,
        attempt_id: String,
    ) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        let form = self
            .discussion_form
            .clone()
            .ok_or_else(|| "No top-level discussion composer is open.".to_owned())?;
        validate_body(&form.body)?;
        let action = match &form.mode {
            DiscussionMode::Create => PullRequestDiscussionAction::Create {
                body: form.body.clone(),
            },
            DiscussionMode::Edit {
                comment,
                selected_author,
            } => {
                if form.body == comment.body {
                    return Err("The comment body did not change.".into());
                }
                PullRequestDiscussionAction::Edit {
                    comment: comment.coordinates.clone(),
                    selected_author: selected_author.clone(),
                    observed_body: comment.body.clone(),
                    observed_updated_at: comment.updated_at.clone(),
                    body: form.body.clone(),
                }
            }
        };
        let summary = match &action {
            PullRequestDiscussionAction::Create { body } => {
                format!("Create top-level comment with exact body {body:?}")
            }
            PullRequestDiscussionAction::Edit {
                comment,
                observed_body,
                body,
                ..
            } => format!(
                "Edit exact comment {}: {:?} → {:?}",
                comment.remote_id, observed_body, body
            ),
            PullRequestDiscussionAction::Delete { .. } => unreachable!(),
        };
        self.confirmation = Some(FrozenMutation::Discussion {
            request: PullRequestDiscussionRequest {
                operation_id,
                attempt_id,
                target: mutation_target(snapshot),
                action,
            },
            body_witness: Some(form.body),
            summary,
        });
        Ok(())
    }

    pub fn prepare_comment_delete(
        &mut self,
        comment: IssueComment,
        operation_id: String,
        attempt_id: String,
    ) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        let author = comment
            .author
            .clone()
            .ok_or_else(|| "A comment without an exact author cannot be deleted.".to_owned())?;
        if !author.eq_ignore_ascii_case(&snapshot.viewer_login) {
            return Err("Only an exact current-user issue comment can be deleted.".into());
        }
        validate_comment_target(&self.repository, self.pull_request, &comment)?;
        let summary = format!(
            "Delete exact comment {} with frozen body {:?}",
            comment.coordinates.remote_id, comment.body
        );
        self.confirmation = Some(FrozenMutation::Discussion {
            request: PullRequestDiscussionRequest {
                operation_id,
                attempt_id,
                target: mutation_target(snapshot),
                action: PullRequestDiscussionAction::Delete {
                    comment: comment.coordinates,
                    selected_author: author,
                    observed_body: comment.body,
                    observed_updated_at: comment.updated_at,
                },
            },
            body_witness: None,
            summary,
        });
        Ok(())
    }

    pub fn take_confirmed(&mut self) -> Result<FrozenMutation, String> {
        let frozen = self
            .confirmation
            .clone()
            .ok_or_else(|| "No lifecycle confirmation is open.".to_owned())?;
        match &frozen {
            FrozenMutation::Lifecycle {
                form_witness: Some(witness),
                ..
            } if self.metadata_form.as_ref() != Some(witness) => {
                return Err(
                    "The visible metadata form changed after confirmation was requested; zero writes sent. Apply again to freeze the new value."
                        .into(),
                );
            }
            FrozenMutation::Discussion {
                body_witness: Some(witness),
                ..
            } if self
                .discussion_form
                .as_ref()
                .is_none_or(|form| &form.body != witness) =>
            {
                return Err(
                    "The visible discussion text changed after confirmation was requested; zero writes sent. Apply again to freeze the new body."
                        .into(),
                );
            }
            _ => {}
        }
        self.active_operation = Some(frozen.operation_id().to_owned());
        Ok(frozen)
    }

    pub fn finish_operation(&mut self, operation_id: &str, acknowledged: bool, notice: String) {
        if self.active_operation.as_deref() != Some(operation_id) {
            return;
        }
        self.active_operation = None;
        self.confirmation = None;
        if acknowledged {
            self.editing_metadata = false;
            self.discussion_form = None;
        }
        self.notice = Some(notice);
    }

    pub fn cancel_confirmation(&mut self) {
        if self.active_operation.is_none() {
            self.confirmation = None;
        }
    }

    pub fn current_user_comment(&self, comment: &IssueComment) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            comment.author.as_deref().is_some_and(|author| {
                author.eq_ignore_ascii_case(&snapshot.viewer_login)
                    && validate_comment_target(&self.repository, self.pull_request, comment).is_ok()
            })
        })
    }

    pub fn choices_for(&self, kind: ChoiceKind) -> Option<(&[ProviderChoice], bool, Option<&str>)> {
        let choices = self.choices.as_ref()?;
        let set = match kind {
            ChoiceKind::Branch => &choices.branches,
            ChoiceKind::Label => &choices.labels,
            ChoiceKind::Assignee => &choices.assignees,
            ChoiceKind::ReviewerUser => &choices.reviewer_users,
            ChoiceKind::ReviewerTeam => &choices.reviewer_teams,
        };
        Some((&set.values, set.complete, set.notice.as_deref()))
    }

    fn snapshot(&self) -> Result<&PullRequestLifecycleSnapshot, String> {
        self.snapshot
            .as_ref()
            .ok_or_else(|| "Lifecycle metadata is still loading.".to_owned())
    }

    fn validate_snapshot(&self, snapshot: &PullRequestLifecycleSnapshot) -> Result<(), String> {
        if snapshot.repository.cache_key() != self.repository.cache_key()
            || snapshot.pull_request.pull_request != self.pull_request
            || !snapshot
                .pull_request
                .owner
                .eq_ignore_ascii_case(&self.repository.owner)
            || !snapshot
                .pull_request
                .repository
                .eq_ignore_ascii_case(&self.repository.name)
            || !snapshot
                .pull_request
                .host
                .eq_ignore_ascii_case(&self.repository.host)
        {
            return Err("Lifecycle snapshot targets another account, repository, or PR.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceKind {
    Branch,
    Label,
    Assignee,
    ReviewerUser,
    ReviewerTeam,
}

fn mutation_target(snapshot: &PullRequestLifecycleSnapshot) -> PullRequestMutationTarget {
    PullRequestMutationTarget {
        repository: snapshot.repository.clone(),
        pull_request: snapshot.pull_request.clone(),
        observed_updated_at: snapshot.updated_at.clone(),
        observed_state: snapshot.state.clone(),
        observed_head_sha: snapshot.head_sha.clone(),
    }
}

fn validate_form(form: &MetadataForm) -> Result<(), String> {
    if form.title.trim().is_empty() {
        return Err("Title cannot be empty.".into());
    }
    if form.base_branch.trim().is_empty() {
        return Err("Base branch cannot be empty.".into());
    }
    if form.title.len() > MAX_FORM_BYTES
        || form.body.len() > MAX_FORM_BYTES
        || form.base_branch.len() > MAX_FORM_BYTES
    {
        return Err("Metadata form exceeds its bounded size.".into());
    }
    Ok(())
}

fn validate_body(body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("Comment body cannot be empty.".into());
    }
    if body.len() > MAX_FORM_BYTES {
        return Err("Comment body exceeds its bounded size.".into());
    }
    Ok(())
}

fn validate_action_capability(
    snapshot: &PullRequestLifecycleSnapshot,
    action: &PullRequestLifecycleAction,
) -> Result<(), String> {
    let (label, capability) = match action {
        PullRequestLifecycleAction::UpdateTitle { .. }
        | PullRequestLifecycleAction::UpdateBody { .. }
        | PullRequestLifecycleAction::UpdateBaseBranch { .. } => {
            ("Metadata change", &snapshot.can_update_metadata)
        }
        PullRequestLifecycleAction::Close | PullRequestLifecycleAction::Reopen => {
            ("State change", &snapshot.can_change_state)
        }
        PullRequestLifecycleAction::ConvertToDraft
        | PullRequestLifecycleAction::MarkReadyForReview => {
            ("Draft-state change", &snapshot.can_change_draft)
        }
        PullRequestLifecycleAction::AddReviewer(_)
        | PullRequestLifecycleAction::RemoveReviewer(_) => {
            ("Reviewer change", &snapshot.can_request_reviewers)
        }
        PullRequestLifecycleAction::AddLabel(_) | PullRequestLifecycleAction::RemoveLabel(_) => {
            ("Label change", &snapshot.can_change_labels)
        }
        PullRequestLifecycleAction::AddAssignee(_)
        | PullRequestLifecycleAction::RemoveAssignee(_) => {
            ("Assignee change", &snapshot.can_change_assignees)
        }
    };
    if capability.available {
        Ok(())
    } else {
        Err(capability_reason(label, &capability.reason))
    }
}

fn capability_reason(label: &str, reason: &Option<String>) -> String {
    format!(
        "{label}: {}",
        reason.as_deref().unwrap_or(
            "the selected-account capability read was incomplete; refresh before retrying"
        )
    )
}

fn lifecycle_summary(action: &PullRequestLifecycleAction) -> String {
    match action {
        PullRequestLifecycleAction::UpdateTitle { observed, value } => {
            format!("Title: {observed:?} → {value:?}")
        }
        PullRequestLifecycleAction::UpdateBody { observed, value } => {
            format!("Body: {observed:?} → {value:?}")
        }
        PullRequestLifecycleAction::UpdateBaseBranch { observed, value } => {
            format!("Base branch: {observed:?} → {value:?}")
        }
        PullRequestLifecycleAction::Close => "Close this exact open PR snapshot".into(),
        PullRequestLifecycleAction::Reopen => "Reopen this exact closed PR snapshot".into(),
        PullRequestLifecycleAction::ConvertToDraft => {
            "Convert this exact ready PR snapshot to draft".into()
        }
        PullRequestLifecycleAction::MarkReadyForReview => {
            "Mark this exact draft PR snapshot ready for review".into()
        }
        PullRequestLifecycleAction::AddReviewer(reviewer) => {
            format!("Add {} reviewer {}", reviewer.kind, reviewer.name)
        }
        PullRequestLifecycleAction::RemoveReviewer(reviewer) => {
            format!("Remove {} reviewer {}", reviewer.kind, reviewer.name)
        }
        PullRequestLifecycleAction::AddLabel(label) => format!("Add label {label:?}"),
        PullRequestLifecycleAction::RemoveLabel(label) => format!("Remove label {label:?}"),
        PullRequestLifecycleAction::AddAssignee(login) => format!("Add assignee {login}"),
        PullRequestLifecycleAction::RemoveAssignee(login) => format!("Remove assignee {login}"),
    }
}

fn validate_comment_target(
    repository: &Repository,
    pull_request: u64,
    comment: &IssueComment,
) -> Result<(), String> {
    let coordinates = &comment.coordinates;
    if coordinates.provider != "github"
        || !coordinates.host.eq_ignore_ascii_case(&repository.host)
        || !coordinates.owner.eq_ignore_ascii_case(&repository.owner)
        || !coordinates
            .repository
            .eq_ignore_ascii_case(&repository.name)
        || coordinates.pull_request != pull_request
        || coordinates.remote_id.is_empty()
    {
        return Err("Comment coordinates do not exactly match this pull request.".into());
    }
    Ok(())
}

pub fn reviewer(kind: &str, name: String) -> PullRequestReviewer {
    PullRequestReviewer {
        kind: kind.into(),
        name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{Account, ProviderCapability, ProviderChoiceSet, ProviderCoordinates};

    fn repository() -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "octo".into(),
            name: "repo".into(),
            account: Account {
                host: "github.com".into(),
                login: "alice".into(),
            },
            local_path: None,
        }
    }

    fn capability() -> ProviderCapability {
        ProviderCapability {
            available: true,
            reason: None,
        }
    }

    fn snapshot() -> PullRequestLifecycleSnapshot {
        PullRequestLifecycleSnapshot {
            repository: repository(),
            pull_request: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "octo".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "PR_7".into(),
            },
            updated_at: "2026-09-13T00:00:00Z".into(),
            state: "OPEN".into(),
            head_sha: "head".into(),
            title: "old title".into(),
            body: "old body".into(),
            base_branch: "main".into(),
            draft: false,
            reviewers: vec![],
            assignees: vec![],
            labels: vec![],
            viewer_login: "alice".into(),
            viewer_permission: Some("WRITE".into()),
            can_update_metadata: capability(),
            can_change_state: capability(),
            can_change_draft: capability(),
            can_request_reviewers: capability(),
            can_change_labels: capability(),
            can_change_assignees: capability(),
            can_comment: capability(),
            values_complete: true,
            capabilities_complete: true,
            notice: None,
        }
    }

    fn comment(id: &str, author: &str, body: &str) -> IssueComment {
        IssueComment {
            coordinates: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "octo".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: id.into(),
            },
            author: Some(author.into()),
            body: body.into(),
            created_at: "created".into(),
            updated_at: "updated".into(),
            url: String::new(),
        }
    }

    #[test]
    fn metadata_confirmation_freezes_one_exact_delta_and_rechecks_visible_form() {
        let mut controller = PrLifecycleController::new(repository(), 7);
        controller.install_snapshot(snapshot()).unwrap();
        controller.begin_metadata_edit().unwrap();
        controller.stage_metadata("new title".into(), "old body".into(), "main".into());
        controller
            .prepare_metadata_apply("op".into(), "attempt".into())
            .unwrap();
        let FrozenMutation::Lifecycle { request, .. } = controller.confirmation.clone().unwrap()
        else {
            panic!("lifecycle confirmation")
        };
        assert!(matches!(
            request.action,
            PullRequestLifecycleAction::UpdateTitle { ref observed, ref value }
                if observed == "old title" && value == "new title"
        ));
        controller.stage_metadata(
            "changed after prompt".into(),
            "old body".into(),
            "main".into(),
        );
        assert!(controller.take_confirmed().unwrap_err().contains("changed"));
        assert!(controller.active_operation.is_none());
    }

    #[test]
    fn dirty_forms_survive_fresh_poll_and_wrong_target_is_rejected() {
        let mut controller = PrLifecycleController::new(repository(), 7);
        controller.install_snapshot(snapshot()).unwrap();
        controller.begin_metadata_edit().unwrap();
        controller.stage_metadata("dirty".into(), "exact\nbody".into(), "main".into());
        let mut fresh = snapshot();
        fresh.title = "remote title".into();
        controller.install_snapshot(fresh).unwrap();
        assert_eq!(controller.metadata_form.as_ref().unwrap().title, "dirty");
        let mut wrong = snapshot();
        wrong.pull_request.pull_request = 8;
        assert!(controller.install_snapshot(wrong).is_err());
    }

    #[test]
    fn exact_comment_identity_and_body_are_frozen_without_body_guessing() {
        let mut controller = PrLifecycleController::new(repository(), 7);
        controller.install_snapshot(snapshot()).unwrap();
        assert!(
            controller
                .begin_comment_edit(comment("C1", "bob", "same"))
                .is_err()
        );
        controller
            .begin_comment_edit(comment("C1", "ALICE", "old"))
            .unwrap();
        controller.stage_discussion_body("new".into());
        controller
            .prepare_discussion_apply("op".into(), "attempt".into())
            .unwrap();
        let FrozenMutation::Discussion { request, .. } = controller.take_confirmed().unwrap()
        else {
            panic!("discussion confirmation")
        };
        assert!(matches!(
            request.action,
            PullRequestDiscussionAction::Edit { ref comment, ref observed_body, ref body, .. }
                if comment.remote_id == "C1" && observed_body == "old" && body == "new"
        ));
    }

    #[test]
    fn choices_preserve_user_team_distinction_and_incomplete_reason() {
        let mut controller = PrLifecycleController::new(repository(), 7);
        let incomplete = ProviderChoiceSet {
            values: vec![ProviderChoice {
                remote_id: Some("T1".into()),
                name: "core".into(),
            }],
            complete: false,
            notice: Some("bounded read".into()),
        };
        let complete = ProviderChoiceSet {
            values: vec![],
            complete: true,
            notice: None,
        };
        controller
            .install_choices(PullRequestLifecycleChoices {
                repository: repository(),
                branches: complete.clone(),
                labels: complete.clone(),
                assignees: complete.clone(),
                reviewer_users: complete,
                reviewer_teams: incomplete,
            })
            .unwrap();
        let (teams, complete, notice) = controller.choices_for(ChoiceKind::ReviewerTeam).unwrap();
        assert_eq!(teams[0].name, "core");
        assert!(!complete);
        assert_eq!(notice, Some("bounded read"));
        assert_eq!(reviewer("TEAM", "core".into()).kind, "TEAM");
    }
}
