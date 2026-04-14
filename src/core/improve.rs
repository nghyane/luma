use crate::core::audit::audit_show;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub route: String,
    pub confidence: String,
    pub affected_layer: String,
    pub target_layers: Vec<String>,
    pub reason: String,
    pub note: String,
    pub suggested_validation: String,
}

/// Build a first-pass heuristic proposal from one audited session.
pub fn propose_from_session(session_id: &str) -> Option<Proposal> {
    let detail = audit_show(session_id)?;
    let has = |name: &str| detail.failure_types.iter().any(|failure| failure == name);

    if has("wrong_source") || has("premature_external_research") {
        return Some(proposal_for_source_failures(&detail.task_family));
    }

    if has("missing_verification") {
        return Some(proposal_for_verification_failures(&detail.task_family));
    }

    Some(Proposal {
        route: "no_action".into(),
        confidence: "low".into(),
        affected_layer: "none".into(),
        target_layers: vec![],
        reason: format!(
            "No current heuristic route matched this session in task family '{}'.",
            detail.task_family
        ),
        note: "Inspect the session manually or extend the taxonomy before making changes.".into(),
        suggested_validation: "No validation needed until a clearer failure pattern is found."
            .into(),
    })
}

fn proposal_for_source_failures(task_family: &str) -> Proposal {
    match task_family {
        "local_audit" => Proposal {
            route: "patch".into(),
            confidence: "medium-high".into(),
            affected_layer: "developer instruction".into(),
            target_layers: vec![
                "developer instruction".into(),
                "tool boundary descriptions".into(),
                "audit workflow policy".into(),
            ],
            reason: "A local audit session escalated to remote lookup before exhausting repo-local evidence, which weakens session-first audit quality.".into(),
            note: "Patch audit-mode guidance first. Emphasize local sessions, local files, and compact evidence packets before external prior art.".into(),
            suggested_validation: "Re-run the same audit-style task and confirm the agent inspects local repo/session artifacts before any GitHub or web lookup.".into(),
        },
        "docs" => Proposal {
            route: "patch".into(),
            confidence: "medium".into(),
            affected_layer: "tool boundary descriptions".into(),
            target_layers: vec![
                "tool boundary descriptions".into(),
                "developer instruction".into(),
            ],
            reason: "A documentation-oriented task mixed local inspection with remote lookups in a way that suggests unclear source-selection guidance.".into(),
            note: "Keep the fix narrow. Clarify when remote documentation is actually required versus when local files are already sufficient.".into(),
            suggested_validation: "Re-run the same docs task and confirm remote lookups only occur after local sources are checked and found insufficient.".into(),
        },
        _ => Proposal {
            route: "patch".into(),
            confidence: "medium".into(),
            affected_layer: "tool boundary descriptions".into(),
            target_layers: vec![
                "tool boundary descriptions".into(),
                "developer instruction".into(),
            ],
            reason: "The session mixed local-repo inspection with remote lookups in a task classified as local-first.".into(),
            note: "Patch the nearest layer first. Prefer clarifying local-first source selection before changing broader prompt assembly.".into(),
            suggested_validation: "Re-run the same task pattern and confirm local file reads happen before any remote lookup.".into(),
        },
    }
}

fn proposal_for_verification_failures(task_family: &str) -> Proposal {
    match task_family {
        "local_code" => Proposal {
            route: "patch".into(),
            confidence: "medium-high".into(),
            affected_layer: "developer instruction".into(),
            target_layers: vec![
                "developer instruction".into(),
                "system prompt assembly".into(),
            ],
            reason: "A code-changing session appears to contain edits without a recognized verification step before the final response.".into(),
            note: "Strengthen code-task verification guidance first. Require an explicit build, test, or lint signal before the agent reports completion.".into(),
            suggested_validation: "Add a regression scenario that edits code and requires build, test, or lint before completion.".into(),
        },
        "verification" => Proposal {
            route: "proposal".into(),
            confidence: "medium".into(),
            affected_layer: "audit workflow policy".into(),
            target_layers: vec![
                "audit workflow policy".into(),
                "developer instruction".into(),
            ],
            reason: "A verification-oriented task still lacked a clear verification signal, suggesting the workflow is not enforcing its own contract.".into(),
            note: "Treat this as a workflow issue, not just wording. The task family itself expects verification to be first-class.".into(),
            suggested_validation: "Create a regression check that fails if a verification-task session ends without a recognized verify command.".into(),
        },
        _ => Proposal {
            route: "patch".into(),
            confidence: "medium".into(),
            affected_layer: "developer instruction".into(),
            target_layers: vec![
                "developer instruction".into(),
                "system prompt assembly".into(),
            ],
            reason: "The session appears to contain code edits without a recognized verification step before the final response.".into(),
            note: "Confirm whether verification was genuinely absent before escalating to broader prompt changes.".into(),
            suggested_validation: "Add a regression scenario that edits code and requires build, test, or lint before completion.".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_failures_route_to_audit_workflow_for_local_audit() {
        let proposal = proposal_for_source_failures("local_audit");
        assert_eq!(proposal.route, "patch");
        assert_eq!(proposal.affected_layer, "developer instruction");
        assert!(
            proposal
                .target_layers
                .iter()
                .any(|layer| layer == "audit workflow policy")
        );
    }

    #[test]
    fn verification_failures_strengthen_code_task_verification() {
        let proposal = proposal_for_verification_failures("local_code");
        assert_eq!(proposal.route, "patch");
        assert_eq!(proposal.affected_layer, "developer instruction");
        assert!(proposal.reason.contains("code-changing session"));
    }
}
