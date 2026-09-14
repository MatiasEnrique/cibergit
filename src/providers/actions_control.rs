//! Explicit GitHub Actions run controls for one exactly identified run.
//!
//! Three controls exist: re-run all jobs, re-run only failed jobs, and a normal
//! cancel. Force-cancel and arbitrary workflow dispatch are deliberately absent.
//!
//! GitHub exposes no expected-state condition on any of the three endpoints, so
//! a control is never atomic. The identity, attempt, status, and repository
//! permission are read freshly before preparation and again after durable
//! admission, and the exact frozen method/path/body is the only thing sent.
//! Only the single documented accepted status is an acknowledgement, and that
//! acknowledgement means GitHub accepted the request, never that a new attempt
//! started or that the run is cancelled. Every other framed status, every
//! unframed response, and every post-send local failure remains an exact-context
//! Uncertain outcome that is never automatically replayed.

use super::{
    API_VERSION, GithubProvider, HOST, MAX_MUTATION_INPUT_BYTES, MutationAdmission,
    RunnerFailureKind, Session, conditional, general_sync::GeneralReadDirective, rejected,
    validate_action_identity, validate_node_id, validate_sha,
};
use crate::domain::{
    ActionsAttemptLocator, ActionsRunControlAcknowledgement, ActionsRunControlAction,
    ActionsRunControlAuthority, ActionsRunControlObservation, ActionsRunControlPreparation,
    ActionsRunControlProgress, ActionsRunControlRequest, ActionsRunControlTarget, MutationContext,
    MutationTerminalRecord, ProviderMutationOutcome, ProviderReadEvidence, Repository,
    SelectedViewer,
};
use serde::Deserialize;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_RUN_CONTROL_TEXT: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 128 * 1024;

/// Statuses in which GitHub can accept a normal cancel.
const CANCELLABLE: [&str; 5] = ["queued", "in_progress", "waiting", "requested", "pending"];

/// Conclusions that cannot have produced a failed job to re-run.
const NO_FAILED_JOBS: [&str; 2] = ["success", "skipped"];

/// One dispatch attempt and the server pacing directives it disclosed.
///
/// The directive is returned even when the outcome is rejected or uncertain so
/// the caller can install the same account floor a read would have installed.
/// Mutation dispatch itself never passes through the automatic read scheduler.
pub struct ActionsRunControlDispatch {
    pub outcome: ProviderMutationOutcome<ActionsRunControlAcknowledgement>,
    pub directive: GeneralReadDirective,
}

impl GithubProvider {
    /// Read the selected viewer, fresh repository permission, and the exact
    /// current run, then freeze one outgoing control request.
    ///
    /// The locator's attempt must still be GitHub's current attempt. A run that
    /// has advanced is refused by name; a historical display never silently
    /// retargets a later attempt.
    pub fn prepare_actions_run_control(
        &self,
        repo: &Repository,
        locator: &ActionsAttemptLocator,
        action: ActionsRunControlAction,
        operation_id: String,
        attempt_id: String,
    ) -> Result<ActionsRunControlRequest, String> {
        validate_action_identity(&operation_id, "operation_id")?;
        validate_action_identity(&attempt_id, "attempt_id")?;
        self.validate_repo(repo)
            .map_err(|error| error.to_string())?;
        if locator.account != self.account || locator.account != repo.account {
            return Err("the Actions run belongs to another selected account".into());
        }
        let observation = self.observe_run_control(repo, locator)?;
        let mut notices = Vec::new();
        if let ActionsRunControlAuthority::Unknown { reason } = &observation.authority {
            notices.push(format!(
                "GitHub returned no usable write-permission evidence ({reason}). GitHub decides authorization when the request is sent."
            ));
        }
        validate_action_against_run(action, &observation)?;
        notices.push(
            "GitHub has no expected-attempt or expected-status condition on this endpoint, so the run can still move between this read and the request."
                .into(),
        );
        let path = control_path(repo, locator.workflow_run.database_id, action);
        Ok(ActionsRunControlRequest {
            operation_id,
            attempt_id,
            preparation: ActionsRunControlPreparation {
                action,
                observation,
                method: "POST".into(),
                path,
                body: json!({}),
                observed_at_unix_ms: now_unix_ms()?,
                notices,
            },
        })
    }

    /// Hold durable per-target authority, repeat the exact preflight, and send
    /// one POST. This method never retries and never replays.
    pub fn execute_actions_run_control(
        &self,
        repo: &Repository,
        request: &ActionsRunControlRequest,
        admission: &mut impl MutationAdmission,
    ) -> ActionsRunControlDispatch {
        let mut directive = GeneralReadDirective::default();
        if let Err(reason) = validate_request(self, repo, request) {
            return ActionsRunControlDispatch {
                outcome: rejected(reason),
                directive,
            };
        }
        let context = mutation_context(request);
        let mut attempt = match admission.admit(&context) {
            Ok(attempt) => attempt,
            Err(error) => {
                return ActionsRunControlDispatch {
                    outcome: rejected(format!(
                        "durable mutation admission failed; dispatched zero writes: {error}"
                    )),
                    directive,
                };
            }
        };
        let receipt = attempt.receipt();
        if receipt.operation_id != request.operation_id
            || receipt.attempt_id != request.attempt_id
            || receipt.durable_record_id.is_empty()
            || receipt.durable_record_id.len() > 1024
            || receipt.durable_record_id.chars().any(char::is_control)
        {
            return ActionsRunControlDispatch {
                outcome: not_started(
                    &context,
                    &mut *attempt,
                    "durable admission receipt does not match the frozen Actions control attempt"
                        .into(),
                ),
                directive,
            };
        }
        let preparation = &request.preparation;
        let fresh = match self.observe_run_control_by_target(repo, &preparation.observation.target)
        {
            Ok(fresh) => fresh,
            Err(reason) => {
                return ActionsRunControlDispatch {
                    outcome: not_started(
                        &context,
                        &mut *attempt,
                        format!("post-admission Actions run preflight failed: {reason}"),
                    ),
                    directive,
                };
            }
        };
        if fresh != preparation.observation {
            return ActionsRunControlDispatch {
                outcome: not_started(
                    &context,
                    &mut *attempt,
                    "post-admission run identity, attempt, status, viewer, or write permission changed; zero writes sent"
                        .into(),
                ),
                directive,
            };
        }
        if let Err(reason) = validate_action_against_run(preparation.action, &fresh) {
            return ActionsRunControlDispatch {
                outcome: not_started(
                    &context,
                    &mut *attempt,
                    format!("post-admission Actions control refused the write: {reason}"),
                ),
                directive,
            };
        }
        let sent = self.send_run_control(preparation);
        directive.merge(&sent.directive);
        let accepted_status = match sent.classification {
            DispatchClassification::NotSent(reason) => {
                return ActionsRunControlDispatch {
                    outcome: not_started(&context, &mut *attempt, reason),
                    directive,
                };
            }
            DispatchClassification::Unresolved(reason) => {
                return ActionsRunControlDispatch {
                    outcome: uncertain(&context, &mut *attempt, reason),
                    directive,
                };
            }
            DispatchClassification::Accepted(status) => status,
        };
        let observed_after = match self.observe_progress(repo, &preparation.observation.target) {
            Ok(progress) => ProviderReadEvidence::Observed(progress),
            Err(reason) => ProviderReadEvidence::Inconclusive { reason },
        };
        let acknowledgement = ActionsRunControlAcknowledgement {
            operation_id: request.operation_id.clone(),
            action: preparation.action,
            target: preparation.observation.target.clone(),
            accepted_status,
            observed_after,
        };
        let encoded = match serde_json::to_value(&acknowledgement) {
            Ok(value) => value,
            Err(error) => {
                return ActionsRunControlDispatch {
                    outcome: uncertain(
                        &context,
                        &mut *attempt,
                        format!(
                            "GitHub accepted the request but its acknowledgement could not be encoded: {error}"
                        ),
                    ),
                    directive,
                };
            }
        };
        if let Err(error) = attempt.record_terminal(&MutationTerminalRecord::Acknowledged {
            acknowledgement: encoded,
        }) {
            return ActionsRunControlDispatch {
                outcome: ProviderMutationOutcome::Uncertain {
                    context,
                    reason: format!(
                        "GitHub accepted the request but its durable terminal acknowledgement could not be saved; durable InFlight authority was retained: {error}"
                    ),
                },
                directive,
            };
        }
        ActionsRunControlDispatch {
            outcome: ProviderMutationOutcome::Acknowledged(acknowledgement),
            directive,
        }
    }

    /// Read-only evidence for one unresolved Actions control attempt.
    ///
    /// A later attempt or a cancelled conclusion is disclosed as observed
    /// movement. It is never attributed to this exact request, and absence of
    /// movement is never recorded as a proven no-op.
    pub fn reconcile_actions_run_control(
        &self,
        repo: &Repository,
        target: &ActionsRunControlTarget,
    ) -> ProviderReadEvidence<ActionsRunControlProgress> {
        match self.observe_progress(repo, target) {
            Ok(progress) => ProviderReadEvidence::Observed(progress),
            Err(reason) => ProviderReadEvidence::Inconclusive { reason },
        }
    }

    fn observe_run_control(
        &self,
        repo: &Repository,
        locator: &ActionsAttemptLocator,
    ) -> Result<ActionsRunControlObservation, String> {
        let mut session = Session::new(self);
        let viewer = read_viewer(&mut session, self)?;
        let repository = read_repository(&mut session, repo)?;
        let run: ApiCurrentRun = session
            .get(&run_path(repo, locator.workflow_run.database_id))
            .map_err(|error| format!("exact Actions run read failed: {error}"))?;
        let target = validated_target(repo, locator, &repository, &run)?;
        Ok(ActionsRunControlObservation {
            target,
            viewer,
            run_status: bounded(&run.status, "run status")?,
            run_conclusion: run
                .conclusion
                .as_deref()
                .map(|value| bounded(value, "run conclusion"))
                .transpose()?,
            authority: repository.authority,
        })
    }

    /// The post-admission preflight. It rebuilds the same observation shape
    /// from a frozen target so the two values can be compared whole.
    fn observe_run_control_by_target(
        &self,
        repo: &Repository,
        target: &ActionsRunControlTarget,
    ) -> Result<ActionsRunControlObservation, String> {
        let mut session = Session::new(self);
        let viewer = read_viewer(&mut session, self)?;
        let repository = read_repository(&mut session, repo)?;
        let run: ApiCurrentRun = session
            .get(&run_path(repo, target.run_database_id))
            .map_err(|error| format!("exact Actions run read failed: {error}"))?;
        let fresh = target_from_run(repo, target, &repository, &run)?;
        Ok(ActionsRunControlObservation {
            target: fresh,
            viewer,
            run_status: bounded(&run.status, "run status")?,
            run_conclusion: run
                .conclusion
                .as_deref()
                .map(|value| bounded(value, "run conclusion"))
                .transpose()?,
            authority: repository.authority,
        })
    }

    fn observe_progress(
        &self,
        repo: &Repository,
        target: &ActionsRunControlTarget,
    ) -> Result<ActionsRunControlProgress, String> {
        let mut session = Session::new(self);
        let run: ApiCurrentRun = session
            .get(&run_path(repo, target.run_database_id))
            .map_err(|error| format!("exact Actions run read failed: {error}"))?;
        if run.id != target.run_database_id
            || run.node_id != target.run_node_id
            || run.run_number != target.run_number
            || run.repository.full_name != target.repository_name_with_owner
        {
            return Err("the observed run identity no longer matches the frozen target".into());
        }
        Ok(ActionsRunControlProgress {
            run_attempt: run.run_attempt,
            run_status: bounded(&run.status, "run status")?,
            run_conclusion: run
                .conclusion
                .as_deref()
                .map(|value| bounded(value, "run conclusion"))
                .transpose()?,
        })
    }

    /// Send the exact frozen method, path, and body once.
    fn send_run_control(&self, preparation: &ActionsRunControlPreparation) -> SentRunControl {
        let input = match serde_json::to_vec(&preparation.body) {
            Ok(input) if input.len() <= MAX_MUTATION_INPUT_BYTES => input,
            Ok(_) | Err(_) => {
                return SentRunControl {
                    classification: DispatchClassification::NotSent(
                        "the frozen Actions control body could not be encoded within its bound; zero writes sent"
                            .into(),
                    ),
                    directive: GeneralReadDirective::default(),
                };
            }
        };
        let token = match Session::new(self).credential() {
            Ok(token) => token,
            Err(error) => {
                return SentRunControl {
                    classification: DispatchClassification::NotSent(format!(
                        "{error}; zero writes sent"
                    )),
                    directive: GeneralReadDirective::default(),
                };
            }
        };
        let mut command = self.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--hostname",
            HOST,
            "--method",
            preparation.method.as_str(),
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            "--include",
            preparation.path.as_str(),
            "--input",
            "-",
        ]);
        let output = match self.runner.run_with_input_status(
            command,
            "send one GitHub Actions run control",
            &input,
        ) {
            Ok(output) => output,
            Err(error) => {
                let kind = error
                    .downcast_ref::<super::RunnerFailure>()
                    .map(|failure| failure.kind);
                return SentRunControl {
                    classification: match kind {
                        // The transport never started, so nothing was sent.
                        Some(RunnerFailureKind::Start) => DispatchClassification::NotSent(
                            "the Actions control transport could not start; zero writes sent"
                                .into(),
                        ),
                        _ => DispatchClassification::Unresolved(
                            "the Actions control request was started but no response was read"
                                .into(),
                        ),
                    },
                    directive: GeneralReadDirective::default(),
                };
            }
        };
        if output.stdout.len() > MAX_RESPONSE_BYTES {
            return SentRunControl {
                classification: DispatchClassification::Unresolved(
                    "the Actions control response exceeded its bound and could not be classified"
                        .into(),
                ),
                directive: GeneralReadDirective::default(),
            };
        }
        let framing = conditional::parse_mutation_response(&output.stdout);
        let directive = framing.poll.clone();
        let Some(status) = framing.status else {
            return SentRunControl {
                classification: DispatchClassification::Unresolved(
                    "the Actions control response could not be framed; its effect is unknown"
                        .into(),
                ),
                directive,
            };
        };
        let expected = preparation.action.accepted_status();
        if status != expected {
            return SentRunControl {
                // A status the request reached GitHub to receive is never
                // recorded as a proven no-op: there is no per-request evidence
                // that separates a refusal from a refusal after an effect.
                classification: DispatchClassification::Unresolved(format!(
                    "GitHub returned status {status} instead of the documented {expected}; the request crossed the transport, so its effect is not proven either way"
                )),
                directive,
            };
        }
        if !body_is_empty_acknowledgement(&framing.body) {
            return SentRunControl {
                classification: DispatchClassification::Unresolved(format!(
                    "GitHub returned the documented status {status} with an unexpected body shape"
                )),
                directive,
            };
        }
        SentRunControl {
            classification: DispatchClassification::Accepted(status),
            directive,
        }
    }
}

struct SentRunControl {
    classification: DispatchClassification,
    directive: GeneralReadDirective,
}

enum DispatchClassification {
    /// Locally proven: the request never crossed the transport.
    NotSent(String),
    /// The request may or may not have taken effect. Never replayed.
    Unresolved(String),
    Accepted(u16),
}

fn not_started<T>(
    context: &MutationContext,
    attempt: &mut dyn super::AdmittedMutationAttempt,
    reason: String,
) -> ProviderMutationOutcome<T> {
    let record_error = attempt
        .record_terminal(&MutationTerminalRecord::NotStarted {
            reason: reason.clone(),
        })
        .err();
    match record_error {
        None => ProviderMutationOutcome::PreflightRejected {
            reason: format!("{reason}; dispatched zero writes"),
        },
        // A terminal record that cannot be saved leaves durable InFlight
        // authority in place, so the attempt stays unresolved rather than
        // becoming a clean rejection.
        Some(error) => ProviderMutationOutcome::Uncertain {
            context: context.clone(),
            reason: format!(
                "{reason}; dispatched zero writes, but the durable NotStarted record failed and InFlight authority was retained: {error}"
            ),
        },
    }
}

fn uncertain<T>(
    context: &MutationContext,
    attempt: &mut dyn super::AdmittedMutationAttempt,
    reason: String,
) -> ProviderMutationOutcome<T> {
    let record_error = attempt
        .record_terminal(&MutationTerminalRecord::Uncertain {
            reason: reason.clone(),
        })
        .err();
    ProviderMutationOutcome::Uncertain {
        context: context.clone(),
        reason: match record_error {
            None => reason,
            Some(error) => format!(
                "{reason}; the durable Uncertain record also failed and InFlight authority was retained: {error}"
            ),
        },
    }
}

/// The single frozen context backing admission, dispatch, and every post-send
/// local failure record.
fn mutation_context(request: &ActionsRunControlRequest) -> MutationContext {
    MutationContext {
        operation_id: request.operation_id.clone(),
        attempt_id: request.attempt_id.clone(),
        action: request.preparation.action.journal_action().into(),
        payload: json!({
            "request": request,
            "dispatch": {
                "transport": "github-rest",
                "method": request.preparation.method,
                "path": request.preparation.path,
                "body": request.preparation.body,
            },
        }),
    }
}

fn control_path(repo: &Repository, run_id: u64, action: ActionsRunControlAction) -> String {
    format!(
        "repos/{}/actions/runs/{run_id}/{}",
        repo.full_name(),
        action.rest_segment()
    )
}

fn run_path(repo: &Repository, run_id: u64) -> String {
    format!("repos/{}/actions/runs/{run_id}", repo.full_name())
}

fn validate_request(
    provider: &GithubProvider,
    repo: &Repository,
    request: &ActionsRunControlRequest,
) -> Result<(), String> {
    validate_action_identity(&request.operation_id, "operation_id")?;
    validate_action_identity(&request.attempt_id, "attempt_id")?;
    provider
        .validate_repo(repo)
        .map_err(|error| error.to_string())?;
    let preparation = &request.preparation;
    let target = &preparation.observation.target;
    if target.account != provider.account || target.account != repo.account {
        return Err("the frozen Actions control targets another selected account".into());
    }
    if !target
        .repository_name_with_owner
        .eq_ignore_ascii_case(&repo.full_name())
    {
        return Err("the frozen Actions control targets another repository".into());
    }
    if preparation.method != "POST" {
        return Err("only POST is supported for Actions run controls".into());
    }
    if preparation.path != control_path(repo, target.run_database_id, preparation.action) {
        return Err(
            "the frozen Actions control path does not match its exact action and run".into(),
        );
    }
    if preparation.body != json!({}) {
        return Err("Actions run controls send an empty JSON body only".into());
    }
    if !preparation.observation.authority.permits_attempt() {
        return Err("the frozen Actions control has no write permission evidence".into());
    }
    validate_node_id(&target.run_node_id).map_err(|error| error.to_string())?;
    validate_sha(&target.run_head_sha).map_err(|error| error.to_string())?;
    Ok(())
}

/// GitHub decides the real precondition; this refuses combinations the
/// documented endpoints cannot accept so an obvious refusal never crosses the
/// transport and leaves an unresolved record behind.
fn validate_action_against_run(
    action: ActionsRunControlAction,
    observation: &ActionsRunControlObservation,
) -> Result<(), String> {
    let status = observation.run_status.as_str();
    match action {
        ActionsRunControlAction::CancelRun => {
            if !CANCELLABLE.contains(&status) {
                return Err(format!(
                    "the run is {status}; a normal cancel applies only to a run that has not completed"
                ));
            }
        }
        ActionsRunControlAction::RerunAllJobs => {
            if status != "completed" {
                return Err(format!(
                    "the run is {status}; GitHub re-runs a workflow run only after it completes"
                ));
            }
        }
        ActionsRunControlAction::RerunFailedJobs => {
            if status != "completed" {
                return Err(format!(
                    "the run is {status}; GitHub re-runs failed jobs only after the run completes"
                ));
            }
            match observation.run_conclusion.as_deref() {
                None => {
                    return Err(
                        "the completed run has no conclusion, so no failed job is identified"
                            .into(),
                    );
                }
                Some(conclusion) if NO_FAILED_JOBS.contains(&conclusion) => {
                    return Err(format!(
                        "the run concluded {conclusion}, so it has no failed job to re-run"
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn body_is_empty_acknowledgement(body: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    let text = text.trim();
    text.is_empty() || text == "{}"
}

fn read_viewer(
    session: &mut Session<'_>,
    provider: &GithubProvider,
) -> Result<SelectedViewer, String> {
    let viewer: ApiViewer = session
        .get("user")
        .map_err(|error| format!("selected viewer read failed: {error}"))?;
    if !viewer.login.eq_ignore_ascii_case(&provider.account.login)
        || viewer.login != provider.account.login
    {
        return Err("the selected GitHub credential resolved to another account".into());
    }
    Ok(SelectedViewer {
        node_id: bounded(&viewer.node_id, "viewer node ID")?,
        login: bounded(&viewer.login, "viewer login")?,
    })
}

struct ObservedRepository {
    node_id: String,
    full_name: String,
    authority: ActionsRunControlAuthority,
}

/// Fresh explicit permission evidence. Identity alone never implies authority.
fn read_repository(
    session: &mut Session<'_>,
    repo: &Repository,
) -> Result<ObservedRepository, String> {
    let observed: ApiRepositoryView = session
        .get(&format!("repos/{}", repo.full_name()))
        .map_err(|error| format!("repository permission read failed: {error}"))?;
    if !observed.full_name.eq_ignore_ascii_case(&repo.full_name()) {
        return Err("the repository permission read returned another repository".into());
    }
    let authority = if observed.archived {
        ActionsRunControlAuthority::Unavailable {
            reason: "the repository is archived and accepts no Actions run control".into(),
        }
    } else {
        match observed.permissions {
            None => ActionsRunControlAuthority::Unknown {
                reason: "the repository read returned no permissions object".into(),
            },
            Some(permissions) if permissions.push || permissions.maintain || permissions.admin => {
                ActionsRunControlAuthority::Available
            }
            Some(_) => ActionsRunControlAuthority::Unavailable {
                reason: "the selected account has no write permission on this repository".into(),
            },
        }
    };
    Ok(ObservedRepository {
        node_id: bounded(&observed.node_id, "repository node ID")?,
        full_name: bounded(&observed.full_name, "repository name")?,
        authority,
    })
}

fn validated_target(
    repo: &Repository,
    locator: &ActionsAttemptLocator,
    repository: &ObservedRepository,
    run: &ApiCurrentRun,
) -> Result<ActionsRunControlTarget, String> {
    let identity = &locator.workflow_run;
    if repository.node_id != locator.base_repository.node_id
        || repository.full_name != locator.base_repository.name_with_owner
    {
        return Err(
            "the fresh repository identity differs from the selected Checks identity".into(),
        );
    }
    if run.id != identity.database_id
        || run.node_id != identity.node_id
        || run.run_number != identity.run_number
        || run.event != identity.event
        || run.workflow_id != identity.workflow_database_id
        || run.check_suite_node_id != locator.suite.node_id
        || locator
            .suite
            .database_id
            .is_some_and(|expected| run.check_suite_id != expected)
        || run.head_sha != locator.check_commit_sha
        || run.url
            != format!(
                "https://api.github.com/{}",
                run_path(repo, identity.database_id)
            )
        || run.html_url != identity.github_url
        || run.workflow_url
            != format!(
                "https://api.github.com/repos/{}/actions/workflows/{}",
                repo.full_name(),
                identity.workflow_database_id
            )
        || run.repository.node_id != locator.base_repository.node_id
        || run.repository.full_name != locator.base_repository.name_with_owner
    {
        return Err("the exact Actions run identity moved and no control was prepared".into());
    }
    let Some(head_repository) = run.head_repository.as_ref() else {
        return Err("the Actions run returned no head repository identity".into());
    };
    if head_repository.node_id != locator.head_repository.node_id
        || head_repository.full_name != locator.head_repository.name_with_owner
    {
        return Err("the Actions run head repository moved and no control was prepared".into());
    }
    if run.run_attempt != identity.run_attempt {
        return Err(format!(
            "the displayed attempt {} is historical: GitHub's current attempt is {}. Reload the exact run attempt before choosing a control; this never retargets a later attempt.",
            identity.run_attempt, run.run_attempt
        ));
    }
    validate_sha(&run.head_sha).map_err(|error| error.to_string())?;
    Ok(ActionsRunControlTarget {
        account: locator.account.clone(),
        repository_node_id: repository.node_id.clone(),
        repository_name_with_owner: repository.full_name.clone(),
        pull_request_number: locator.pull_request_number,
        pull_request_node_id: bounded(&locator.pull_request_node_id, "pull request node ID")?,
        check_node_id: bounded(&locator.check_node_id, "check node ID")?,
        check_database_id: locator.check_database_id,
        check_suite_node_id: bounded(&locator.suite.node_id, "check suite node ID")?,
        check_suite_database_id: run.check_suite_id,
        workflow_node_id: bounded(&identity.workflow_node_id, "workflow node ID")?,
        workflow_database_id: identity.workflow_database_id,
        workflow_name: bounded(&identity.workflow_name, "workflow name")?,
        run_node_id: bounded(&identity.node_id, "run node ID")?,
        run_database_id: identity.database_id,
        run_number: identity.run_number,
        run_attempt: run.run_attempt,
        run_event: bounded(&identity.event, "run event")?,
        run_head_sha: run.head_sha.clone(),
        run_html_url: bounded(&identity.github_url, "run URL")?,
    })
}

/// Rebuild the observed target from a frozen one. Every identity field must
/// match exactly; the caller then compares the whole observation.
fn target_from_run(
    repo: &Repository,
    frozen: &ActionsRunControlTarget,
    repository: &ObservedRepository,
    run: &ApiCurrentRun,
) -> Result<ActionsRunControlTarget, String> {
    if repository.node_id != frozen.repository_node_id
        || repository.full_name != frozen.repository_name_with_owner
    {
        return Err("the fresh repository identity differs from the frozen target".into());
    }
    if run.id != frozen.run_database_id
        || run.node_id != frozen.run_node_id
        || run.run_number != frozen.run_number
        || run.event != frozen.run_event
        || run.workflow_id != frozen.workflow_database_id
        || run.check_suite_node_id != frozen.check_suite_node_id
        || run.check_suite_id != frozen.check_suite_database_id
        || run.head_sha != frozen.run_head_sha
        || run.html_url != frozen.run_html_url
        || run.url
            != format!(
                "https://api.github.com/{}",
                run_path(repo, frozen.run_database_id)
            )
        || run.repository.node_id != frozen.repository_node_id
        || run.repository.full_name != frozen.repository_name_with_owner
    {
        return Err("the exact Actions run identity moved after admission".into());
    }
    if run.run_attempt != frozen.run_attempt {
        return Err(format!(
            "the run advanced to attempt {} after the frozen attempt {}",
            run.run_attempt, frozen.run_attempt
        ));
    }
    let mut refreshed = frozen.clone();
    refreshed.run_attempt = run.run_attempt;
    refreshed.run_head_sha = run.head_sha.clone();
    Ok(refreshed)
}

fn bounded(value: &str, field: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > MAX_RUN_CONTROL_TEXT
        || value.contains('\0')
        || value.chars().any(|character| character.is_control())
    {
        return Err(format!(
            "the observed {field} is empty or exceeds its bound"
        ));
    }
    Ok(value.to_owned())
}

fn now_unix_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or_else(|| "the host clock is unusable for an exact observation time".into())
}

#[derive(Deserialize)]
struct ApiViewer {
    login: String,
    node_id: String,
}

#[derive(Deserialize)]
struct ApiRepositoryPermissions {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    maintain: bool,
    #[serde(default)]
    push: bool,
}

#[derive(Deserialize)]
struct ApiRepositoryView {
    node_id: String,
    full_name: String,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    permissions: Option<ApiRepositoryPermissions>,
}

#[derive(Clone, Deserialize)]
struct ApiRepositoryIdentity {
    node_id: String,
    full_name: String,
}

#[derive(Deserialize)]
struct ApiCurrentRun {
    id: u64,
    node_id: String,
    run_attempt: u64,
    run_number: u64,
    event: String,
    status: String,
    conclusion: Option<String>,
    workflow_id: u64,
    check_suite_id: u64,
    check_suite_node_id: String,
    head_sha: String,
    url: String,
    html_url: String,
    workflow_url: String,
    repository: ApiRepositoryIdentity,
    head_repository: Option<ApiRepositoryIdentity>,
}
