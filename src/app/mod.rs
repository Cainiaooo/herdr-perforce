//! UI-facing state for the explicitly confirmed Submit workflow.
//!
//! The reducer is deliberately independent of terminal rendering and threads.
//! It emits effects for the host to run and rejects late completions that no
//! longer match the visible overlay.

use crate::p4::{
    AuthorizedSubmit, P4ErrorKind, SubmitError, SubmitIntent, SubmitPreview,
    SubmitReconciliationReceipt, SubmitReconciliationResult, SubmitResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcomeCertainty {
    /// No submit command was started for this interaction.
    NotStarted,
    /// Perforce or the local process boundary definitively rejected the write.
    Rejected,
    /// A write may have reached the server; submitting again is unsafe.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitFault {
    Authentication,
    Permission,
    Trust,
    Timeout,
    Network,
    ServerRejected,
    StaleReview,
    Blocked,
    Busy,
    InvalidState,
    Verification,
    ConfirmedPending,
    ExternalTool,
    ExternalHandoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitFailure {
    pub fault: SubmitFault,
    pub certainty: SubmitOutcomeCertainty,
    pub detail: String,
    pub next_step: &'static str,
}

impl SubmitFailure {
    #[must_use]
    pub fn title(&self) -> &'static str {
        if self.fault == SubmitFault::ExternalHandoff {
            return "External submit handoff";
        }
        if self.certainty == SubmitOutcomeCertainty::Unknown {
            return "Submission result unknown";
        }
        match self.fault {
            SubmitFault::Authentication => "Perforce login required",
            SubmitFault::Permission => "Perforce permission denied",
            SubmitFault::Trust => "Perforce trust required",
            SubmitFault::Timeout => "Submit preflight timed out",
            SubmitFault::Network => "Perforce server unavailable",
            SubmitFault::ServerRejected => "Submit rejected",
            SubmitFault::StaleReview => "Submit review is stale",
            SubmitFault::Blocked => "Submit is blocked",
            SubmitFault::Busy => "Another submit is running",
            SubmitFault::InvalidState => "Submit preflight failed",
            SubmitFault::Verification => "Submit verification failed",
            SubmitFault::ConfirmedPending => "Changelist is still pending",
            SubmitFault::ExternalTool => "External submit tool failed",
            SubmitFault::ExternalHandoff => "External submit handoff",
        }
    }

    #[must_use]
    pub const fn requires_read_only_reconciliation(&self) -> bool {
        matches!(self.certainty, SubmitOutcomeCertainty::Unknown)
    }
}

#[derive(Debug)]
pub enum SubmitOverlayState {
    Closed,
    Preflight {
        change: u64,
        request_id: u64,
    },
    Review {
        preview: SubmitPreview,
    },
    Running {
        change: u64,
        receipt: SubmitReconciliationReceipt,
    },
    Reconciling {
        change: u64,
        receipt: SubmitReconciliationReceipt,
        request_id: u64,
    },
    Failure {
        change: u64,
        failure: SubmitFailure,
        receipt: Option<SubmitReconciliationReceipt>,
    },
    Success {
        result: SubmitResult,
        reconciled: bool,
    },
}

#[derive(Debug)]
pub enum SubmitOverlayRequest {
    Preflight {
        change: u64,
        request_id: u64,
    },
    Execute {
        change: u64,
        authorization: AuthorizedSubmit,
    },
    Reconcile {
        receipt: SubmitReconciliationReceipt,
        request_id: u64,
    },
}

#[derive(Debug)]
pub struct SubmitOverlay {
    state: SubmitOverlayState,
    next_request_id: u64,
}

impl Default for SubmitOverlay {
    fn default() -> Self {
        Self {
            state: SubmitOverlayState::Closed,
            next_request_id: 0,
        }
    }
}

impl SubmitOverlay {
    #[must_use]
    pub const fn state(&self) -> &SubmitOverlayState {
        &self.state
    }

    #[cfg(test)]
    pub(crate) fn replace_state_for_test(&mut self, state: SubmitOverlayState) {
        self.state = state;
    }

    fn allocate_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.next_request_id
    }

    pub fn open(&mut self, change: u64) -> Option<SubmitOverlayRequest> {
        if matches!(
            self.state,
            SubmitOverlayState::Running { .. } | SubmitOverlayState::Reconciling { .. }
        ) || matches!(
            &self.state,
            SubmitOverlayState::Failure { failure, .. }
                if failure.requires_read_only_reconciliation()
        ) {
            return None;
        }
        let request_id = self.allocate_request_id();
        self.state = SubmitOverlayState::Preflight { change, request_id };
        Some(SubmitOverlayRequest::Preflight { change, request_id })
    }

    pub fn complete_preflight(
        &mut self,
        change: u64,
        request_id: u64,
        result: Result<SubmitPreview, SubmitError>,
    ) {
        if !matches!(
            self.state,
            SubmitOverlayState::Preflight {
                change: active,
                request_id: active_id
            } if active == change && active_id == request_id
        ) {
            return;
        }
        self.state = match result {
            Ok(preview) => SubmitOverlayState::Review { preview },
            Err(error) => SubmitOverlayState::Failure {
                change,
                failure: classify_failure(change, &error, FailureContext::Preflight),
                receipt: None,
            },
        };
    }

    pub fn handle_intent(&mut self, intent: SubmitIntent) -> Option<SubmitOverlayRequest> {
        let state = std::mem::replace(&mut self.state, SubmitOverlayState::Closed);
        match state {
            SubmitOverlayState::Review { preview }
                if matches!(intent, SubmitIntent::SubmitButton | SubmitIntent::CtrlEnter) =>
            {
                let change = preview.change;
                if let Some(authorization) = preview.clone().authorize(intent) {
                    let receipt = authorization.reconciliation_receipt();
                    self.state = SubmitOverlayState::Running { change, receipt };
                    Some(SubmitOverlayRequest::Execute {
                        change,
                        authorization,
                    })
                } else {
                    self.state = SubmitOverlayState::Review { preview };
                    None
                }
            }
            SubmitOverlayState::Review { .. }
                if matches!(
                    intent,
                    SubmitIntent::Cancel
                        | SubmitIntent::Escape
                        | SubmitIntent::Close
                        | SubmitIntent::LoseFocus
                        | SubmitIntent::Enter
                ) =>
            {
                None
            }
            running @ (SubmitOverlayState::Running { .. }
            | SubmitOverlayState::Reconciling { .. }) => {
                self.state = running;
                None
            }
            SubmitOverlayState::Failure {
                change,
                failure,
                receipt,
            } if failure.requires_read_only_reconciliation() => {
                self.state = SubmitOverlayState::Failure {
                    change,
                    failure,
                    receipt,
                };
                None
            }
            other => {
                if !matches!(
                    intent,
                    SubmitIntent::Cancel
                        | SubmitIntent::Escape
                        | SubmitIntent::Close
                        | SubmitIntent::LoseFocus
                        | SubmitIntent::Enter
                ) {
                    self.state = other;
                }
                None
            }
        }
    }

    pub fn complete_submit(&mut self, change: u64, result: Result<SubmitResult, SubmitError>) {
        let state = std::mem::replace(&mut self.state, SubmitOverlayState::Closed);
        let SubmitOverlayState::Running {
            change: active,
            receipt,
        } = state
        else {
            self.state = state;
            return;
        };
        if active != change {
            self.state = SubmitOverlayState::Running {
                change: active,
                receipt,
            };
            return;
        }
        self.state = match result {
            Ok(result) => SubmitOverlayState::Success {
                result,
                reconciled: false,
            },
            Err(error) => {
                let failure = classify_failure(change, &error, FailureContext::Execute);
                let keep_receipt = failure.requires_read_only_reconciliation();
                SubmitOverlayState::Failure {
                    change,
                    failure,
                    receipt: keep_receipt.then_some(receipt),
                }
            }
        };
    }

    pub fn complete_external_handoff(&mut self, change: u64, result: Result<String, String>) {
        let state = std::mem::replace(&mut self.state, SubmitOverlayState::Closed);
        let SubmitOverlayState::Running {
            change: active,
            receipt,
        } = state
        else {
            self.state = state;
            return;
        };
        if active != change {
            self.state = SubmitOverlayState::Running {
                change: active,
                receipt,
            };
            return;
        }
        self.state = match result {
            Ok(provider) => SubmitOverlayState::Failure {
                change,
                failure: SubmitFailure {
                    fault: SubmitFault::ExternalHandoff,
                    certainty: SubmitOutcomeCertainty::Unknown,
                    detail: format!(
                        "{provider} was opened for CL {change}. Herdr did not run p4 submit and cannot yet confirm the external result."
                    ),
                    next_step: "Complete or cancel the external workflow, then run read-only reconciliation.",
                },
                receipt: Some(receipt),
            },
            Err(detail) => SubmitOverlayState::Failure {
                change,
                failure: SubmitFailure {
                    fault: SubmitFault::ExternalTool,
                    certainty: SubmitOutcomeCertainty::NotStarted,
                    detail,
                    next_step: "Fix the submit provider configuration, then refresh preflight.",
                },
                receipt: None,
            },
        };
    }

    pub fn refresh(&mut self) -> Option<SubmitOverlayRequest> {
        let state = std::mem::replace(&mut self.state, SubmitOverlayState::Closed);
        let SubmitOverlayState::Failure {
            change,
            failure,
            receipt,
        } = state
        else {
            self.state = state;
            return None;
        };
        if failure.requires_read_only_reconciliation() {
            let Some(receipt) = receipt else {
                self.state = SubmitOverlayState::Failure {
                    change,
                    failure,
                    receipt: None,
                };
                return None;
            };
            let request_id = self.allocate_request_id();
            self.state = SubmitOverlayState::Reconciling {
                change,
                receipt: receipt.clone(),
                request_id,
            };
            Some(SubmitOverlayRequest::Reconcile {
                receipt,
                request_id,
            })
        } else {
            let request_id = self.allocate_request_id();
            self.state = SubmitOverlayState::Preflight { change, request_id };
            Some(SubmitOverlayRequest::Preflight { change, request_id })
        }
    }

    pub fn complete_reconciliation(
        &mut self,
        change: u64,
        request_id: u64,
        result: Result<SubmitReconciliationResult, SubmitError>,
    ) {
        let state = std::mem::replace(&mut self.state, SubmitOverlayState::Closed);
        let SubmitOverlayState::Reconciling {
            change: active,
            receipt,
            request_id: active_id,
        } = state
        else {
            self.state = state;
            return;
        };
        if active != change || active_id != request_id {
            self.state = SubmitOverlayState::Reconciling {
                change: active,
                receipt,
                request_id: active_id,
            };
            return;
        }

        self.state = match result {
            Ok(SubmitReconciliationResult::ConfirmedSubmitted(result)) => {
                SubmitOverlayState::Success {
                    result,
                    reconciled: true,
                }
            }
            Ok(SubmitReconciliationResult::ConfirmedPending {
                snapshot_changed, ..
            }) => SubmitOverlayState::Failure {
                change,
                failure: SubmitFailure {
                    fault: SubmitFault::ConfirmedPending,
                    certainty: SubmitOutcomeCertainty::Rejected,
                    detail: if snapshot_changed {
                        "A read-only refresh confirms the changelist is pending and changed since review."
                            .to_owned()
                    } else {
                        "A read-only refresh confirms the changelist is still pending."
                            .to_owned()
                    },
                    next_step: "Run preflight and review a new confirmation before any new submit.",
                },
                receipt: None,
            },
            Ok(SubmitReconciliationResult::Inconclusive) => SubmitOverlayState::Failure {
                change,
                failure: SubmitFailure {
                    fault: SubmitFault::Verification,
                    certainty: SubmitOutcomeCertainty::Unknown,
                    detail: "The read-only refresh could not match the current server state to the submitted review."
                        .to_owned(),
                    next_step: "Inspect the changelist in Perforce. Do not submit again while the result is unknown.",
                },
                receipt: Some(receipt),
            },
            Err(error) => SubmitOverlayState::Failure {
                change,
                failure: classify_failure(change, &error, FailureContext::Reconcile),
                receipt: Some(receipt),
            },
        };
    }
}

#[derive(Debug, Clone, Copy)]
enum FailureContext {
    Preflight,
    Execute,
    Reconcile,
}

fn classify_failure(change: u64, error: &SubmitError, context: FailureContext) -> SubmitFailure {
    let certainty = failure_certainty(error, context);
    let fault = match error {
        SubmitError::Query { source, .. } => fault_from_p4(source.kind),
        SubmitError::TimedOut { .. } => SubmitFault::Timeout,
        SubmitError::Blocked(_) => SubmitFault::Blocked,
        SubmitError::Stale => SubmitFault::StaleReview,
        SubmitError::AlreadyRunning => SubmitFault::Busy,
        SubmitError::VerificationFailed => SubmitFault::Verification,
        SubmitError::Mapping { .. } | SubmitError::InvalidSnapshot => SubmitFault::InvalidState,
    };
    let detail = failure_detail(change, error, fault, certainty, context);
    let next_step = failure_next_step(fault, certainty);
    SubmitFailure {
        fault,
        certainty,
        detail,
        next_step,
    }
}

fn failure_certainty(error: &SubmitError, context: FailureContext) -> SubmitOutcomeCertainty {
    if matches!(context, FailureContext::Preflight) {
        return SubmitOutcomeCertainty::NotStarted;
    }
    if matches!(context, FailureContext::Reconcile) {
        return SubmitOutcomeCertainty::Unknown;
    }
    match error {
        SubmitError::TimedOut { .. } | SubmitError::VerificationFailed => {
            SubmitOutcomeCertainty::Unknown
        }
        SubmitError::Query { stage, source: _ } if stage.starts_with("post-submit") => {
            SubmitOutcomeCertainty::Unknown
        }
        SubmitError::Mapping { stage, .. } if stage.starts_with("post-submit") => {
            SubmitOutcomeCertainty::Unknown
        }
        SubmitError::Query {
            stage: "write",
            source,
        } => match source.kind {
            P4ErrorKind::NetworkUnavailable
            | P4ErrorKind::MalformedOutput
            | P4ErrorKind::TimedOut
            | P4ErrorKind::Cancelled
            | P4ErrorKind::OutputLimitExceeded => SubmitOutcomeCertainty::Unknown,
            _ => SubmitOutcomeCertainty::Rejected,
        },
        SubmitError::Blocked(_)
        | SubmitError::Stale
        | SubmitError::AlreadyRunning
        | SubmitError::InvalidSnapshot
        | SubmitError::Query { .. }
        | SubmitError::Mapping { .. } => SubmitOutcomeCertainty::NotStarted,
    }
}

fn fault_from_p4(kind: P4ErrorKind) -> SubmitFault {
    match kind {
        P4ErrorKind::AuthenticationExpired => SubmitFault::Authentication,
        P4ErrorKind::PermissionDenied => SubmitFault::Permission,
        P4ErrorKind::TrustRequired => SubmitFault::Trust,
        P4ErrorKind::TimedOut => SubmitFault::Timeout,
        P4ErrorKind::NetworkUnavailable => SubmitFault::Network,
        P4ErrorKind::CommandFailed => SubmitFault::ServerRejected,
        P4ErrorKind::ExecutableMissing
        | P4ErrorKind::NotInClientView
        | P4ErrorKind::MalformedOutput
        | P4ErrorKind::Cancelled
        | P4ErrorKind::OutputLimitExceeded => SubmitFault::InvalidState,
    }
}

fn failure_detail(
    change: u64,
    error: &SubmitError,
    fault: SubmitFault,
    certainty: SubmitOutcomeCertainty,
    context: FailureContext,
) -> String {
    if certainty == SubmitOutcomeCertainty::Unknown {
        return match fault {
            SubmitFault::Authentication => format!(
                "Authentication failed while verifying CL {change} after the write was issued."
            ),
            SubmitFault::Permission => format!(
                "Permission was denied while verifying CL {change} after the write was issued."
            ),
            SubmitFault::Timeout => format!(
                "The write or its verification timed out for CL {change}; the server may have accepted it."
            ),
            SubmitFault::Network => format!(
                "The connection failed after Submit began for CL {change}; no final server result was received."
            ),
            _ => format!(
                "Submit began for CL {change}, but the final server state could not be verified."
            ),
        };
    }
    if certainty == SubmitOutcomeCertainty::Rejected && matches!(context, FailureContext::Execute) {
        return match fault {
            SubmitFault::Authentication => format!(
                "Submit ran for CL {change}; Perforce refused the write because authentication is required or expired."
            ),
            SubmitFault::Permission => format!(
                "Submit ran for CL {change}; Perforce refused the write because the current user lacks permission."
            ),
            SubmitFault::Trust => format!(
                "Submit ran for CL {change}; Perforce refused the write pending an explicit trust decision."
            ),
            SubmitFault::ServerRejected => format!(
                "Perforce returned a definite submit rejection for CL {change} after the write command ran; no automatic repair or retry was run."
            ),
            _ => format!(
                "Submit ran for CL {change} and the server refused the write; run a new preflight before trying again."
            ),
        };
    }
    match error {
        SubmitError::Blocked(reason) => format!("Preflight blocked CL {change}: {reason}."),
        SubmitError::Stale => {
            format!("CL {change} changed after this confirmation was created.")
        }
        SubmitError::AlreadyRunning => {
            "A workspace submit is already in progress; no second write was started.".to_owned()
        }
        _ => match fault {
            SubmitFault::Authentication => {
                "Perforce authentication is required or expired; no submit was started.".to_owned()
            }
            SubmitFault::Permission => {
                "The current Perforce user cannot read or submit this changelist.".to_owned()
            }
            SubmitFault::Trust => {
                "The server requires an explicit trust decision before preflight can continue."
                    .to_owned()
            }
            SubmitFault::Timeout => {
                "Preflight timed out before a submit command was started.".to_owned()
            }
            SubmitFault::Network => {
                "The Perforce server was unavailable before a submit command was started.".to_owned()
            }
            SubmitFault::ServerRejected => {
                "Perforce returned a definite submit rejection; no automatic repair or retry was run."
                    .to_owned()
            }
            SubmitFault::Verification => {
                "The refreshed changelist did not match the reviewed submit.".to_owned()
            }
            SubmitFault::InvalidState => {
                "The changelist could not be mapped into a safe Submit review.".to_owned()
            }
            SubmitFault::Blocked
            | SubmitFault::StaleReview
            | SubmitFault::Busy
            | SubmitFault::ConfirmedPending
            | SubmitFault::ExternalTool
            | SubmitFault::ExternalHandoff => unreachable!("constructed outside classification"),
        },
    }
}

fn failure_next_step(fault: SubmitFault, certainty: SubmitOutcomeCertainty) -> &'static str {
    if certainty == SubmitOutcomeCertainty::Unknown {
        return match fault {
            SubmitFault::Authentication => {
                "Restore login, then run read-only reconciliation. Do not submit again."
            }
            SubmitFault::Permission => {
                "Obtain read access or ask an administrator to inspect the CL. Do not submit again."
            }
            _ => "Run read-only reconciliation. Do not submit again while the result is unknown.",
        };
    }
    match fault {
        SubmitFault::Authentication => "Run p4 login in this workspace, then refresh preflight.",
        SubmitFault::Permission => "Request the required Perforce access, then refresh preflight.",
        SubmitFault::Trust => {
            "Verify the server fingerprint outside Herdr, then refresh preflight."
        }
        SubmitFault::Timeout | SubmitFault::Network => {
            "Restore connectivity and explicitly refresh preflight."
        }
        SubmitFault::ServerRejected => {
            "Inspect and fix the reported server condition, then run a new preflight."
        }
        SubmitFault::StaleReview => "Refresh and review the changelist again.",
        SubmitFault::Blocked | SubmitFault::InvalidState => {
            "Fix the blocking changelist condition, then refresh preflight."
        }
        SubmitFault::Busy => "Wait for the active submit to finish, then refresh.",
        SubmitFault::Verification => {
            "Inspect the changelist in Perforce before taking another write action."
        }
        SubmitFault::ConfirmedPending => {
            "Run preflight and review a new confirmation before any new submit."
        }
        SubmitFault::ExternalTool => {
            "Fix the submit provider configuration, then refresh preflight."
        }
        SubmitFault::ExternalHandoff => {
            "Complete or cancel the external workflow, then run read-only reconciliation."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p4::{P4Error, SubmitBlockReason};

    fn query(stage: &'static str, kind: P4ErrorKind) -> SubmitError {
        SubmitError::Query {
            stage,
            source: P4Error::new(kind, "sensitive diagnostic must not surface"),
        }
    }

    #[test]
    fn authentication_permission_and_preflight_timeout_never_claim_a_write_started() {
        for (error, fault) in [
            (
                query("workspace refresh", P4ErrorKind::AuthenticationExpired),
                SubmitFault::Authentication,
            ),
            (
                query("file preflight", P4ErrorKind::PermissionDenied),
                SubmitFault::Permission,
            ),
            (
                query("file preflight", P4ErrorKind::TimedOut),
                SubmitFault::Timeout,
            ),
        ] {
            let failure = classify_failure(42, &error, FailureContext::Preflight);
            assert_eq!(failure.fault, fault);
            assert_eq!(failure.certainty, SubmitOutcomeCertainty::NotStarted);
            assert!(!failure.detail.contains("sensitive"));
        }
    }

    #[test]
    fn write_timeout_and_every_post_submit_failure_are_uncertain() {
        let failures = [
            SubmitError::TimedOut { stage: "write" },
            query(
                "post-submit workspace refresh",
                P4ErrorKind::AuthenticationExpired,
            ),
            query(
                "post-submit changelist refresh",
                P4ErrorKind::PermissionDenied,
            ),
            SubmitError::VerificationFailed,
        ];
        for error in failures {
            let failure = classify_failure(42, &error, FailureContext::Execute);
            assert_eq!(failure.certainty, SubmitOutcomeCertainty::Unknown);
            assert!(failure.requires_read_only_reconciliation());
            assert!(failure.next_step.contains("Do not submit again"));
        }
    }

    #[test]
    fn definite_write_rejections_are_not_mislabeled_unknown() {
        for kind in [
            P4ErrorKind::AuthenticationExpired,
            P4ErrorKind::PermissionDenied,
            P4ErrorKind::CommandFailed,
        ] {
            let error = query("write", kind);
            let failure = classify_failure(42, &error, FailureContext::Execute);
            assert_eq!(failure.certainty, SubmitOutcomeCertainty::Rejected);
            assert!(!failure.requires_read_only_reconciliation());
        }
    }

    #[test]
    fn network_loss_during_write_is_conservatively_unknown() {
        let error = query("write", P4ErrorKind::NetworkUnavailable);
        let failure = classify_failure(42, &error, FailureContext::Execute);
        assert_eq!(failure.fault, SubmitFault::Network);
        assert_eq!(failure.certainty, SubmitOutcomeCertainty::Unknown);
    }

    #[test]
    fn blocked_reason_is_kept_actionable_without_raw_output() {
        let error = SubmitError::Blocked(SubmitBlockReason::UnresolvedFiles);
        let failure = classify_failure(42, &error, FailureContext::Preflight);
        assert_eq!(failure.fault, SubmitFault::Blocked);
        assert!(failure.detail.contains("unresolved"));
        assert_eq!(failure.certainty, SubmitOutcomeCertainty::NotStarted);
    }

    #[test]
    fn unknown_result_cannot_be_closed_or_replaced_with_a_new_submit() {
        let mut overlay = SubmitOverlay::default();
        overlay.replace_state_for_test(SubmitOverlayState::Failure {
            change: 42,
            failure: SubmitFailure {
                fault: SubmitFault::Timeout,
                certainty: SubmitOutcomeCertainty::Unknown,
                detail: "unknown".into(),
                next_step: "reconcile",
            },
            receipt: None,
        });
        assert!(overlay.handle_intent(SubmitIntent::Escape).is_none());
        assert!(matches!(
            overlay.state(),
            SubmitOverlayState::Failure { change: 42, .. }
        ));
        assert!(overlay.open(43).is_none());
        assert!(matches!(
            overlay.state(),
            SubmitOverlayState::Failure { change: 42, .. }
        ));
    }

    #[test]
    fn stale_preflight_completion_cannot_replace_a_newer_request() {
        let mut overlay = SubmitOverlay::default();
        let first = overlay.open(42).expect("first preflight");
        let SubmitOverlayRequest::Preflight {
            request_id: first_id,
            ..
        } = first
        else {
            panic!("open should request preflight");
        };
        overlay.handle_intent(SubmitIntent::Escape);
        let second = overlay.open(42).expect("second preflight");
        let SubmitOverlayRequest::Preflight {
            request_id: second_id,
            ..
        } = second
        else {
            panic!("reopen should request preflight");
        };
        assert_ne!(first_id, second_id);
        overlay.complete_preflight(42, first_id, Err(SubmitError::Stale));
        assert!(matches!(
            overlay.state(),
            SubmitOverlayState::Preflight {
                change: 42,
                request_id,
            } if *request_id == second_id
        ));
        overlay.complete_preflight(42, second_id, Err(SubmitError::Stale));
        assert!(matches!(
            overlay.state(),
            SubmitOverlayState::Failure { change: 42, .. }
        ));
    }

    #[test]
    fn write_stage_rejection_does_not_claim_the_submit_never_started() {
        for kind in [
            P4ErrorKind::AuthenticationExpired,
            P4ErrorKind::PermissionDenied,
            P4ErrorKind::TrustRequired,
            P4ErrorKind::CommandFailed,
        ] {
            let failure = classify_failure(42, &query("write", kind), FailureContext::Execute);
            assert_eq!(failure.certainty, SubmitOutcomeCertainty::Rejected);
            assert!(
                failure.detail.contains("Submit ran")
                    || failure.detail.contains("write command ran")
            );
            assert!(
                !failure
                    .detail
                    .to_ascii_lowercase()
                    .contains("no submit was started")
            );
            assert!(!failure.detail.contains("before preflight"));
        }
    }

    #[test]
    fn explicit_submit_intent_authorizes_without_panicking() {
        let mut overlay = SubmitOverlay::default();
        let preview = SubmitPreview::from_workspace_for_test(
            42,
            "Fix overlay",
            crate::domain::WorkspaceIdentity {
                server_id: "server".into(),
                user: "user".into(),
                client: "client".into(),
                root: std::path::PathBuf::from("C:/Example Workspace"),
                stream: None,
                case_handling: crate::domain::CaseHandling::Insensitive,
            },
        );
        overlay.replace_state_for_test(SubmitOverlayState::Review { preview });
        assert!(matches!(
            overlay.handle_intent(SubmitIntent::SubmitButton),
            Some(SubmitOverlayRequest::Execute { change: 42, .. })
        ));
        assert!(matches!(
            overlay.state(),
            SubmitOverlayState::Running { change: 42, .. }
        ));
    }

    #[test]
    fn external_handoff_never_claims_submit_success() {
        let mut overlay = SubmitOverlay::default();
        let preview = SubmitPreview::from_workspace_for_test(
            42,
            "Fix overlay",
            crate::domain::WorkspaceIdentity {
                server_id: "server".into(),
                user: "user".into(),
                client: "client".into(),
                root: std::path::PathBuf::from("C:/Example Workspace"),
                stream: None,
                case_handling: crate::domain::CaseHandling::Insensitive,
            },
        );
        overlay.replace_state_for_test(SubmitOverlayState::Review { preview });
        assert!(matches!(
            overlay.handle_intent(SubmitIntent::CtrlEnter),
            Some(SubmitOverlayRequest::Execute { change: 42, .. })
        ));
        overlay.complete_external_handoff(42, Ok("P4Lab".to_owned()));
        assert!(matches!(
            overlay.state(),
            SubmitOverlayState::Failure {
                failure: SubmitFailure {
                    fault: SubmitFault::ExternalHandoff,
                    certainty: SubmitOutcomeCertainty::Unknown,
                    ..
                },
                receipt: Some(_),
                ..
            }
        ));
        assert!(overlay.open(43).is_none());
        assert!(matches!(
            overlay.refresh(),
            Some(SubmitOverlayRequest::Reconcile { .. })
        ));
    }
}
