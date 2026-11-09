//! Exact snapshots for the inert contract seam, not behavior-gap evidence.
use super::*;
use crate::agent::{ProgramBlockKind, ProgramState};
use crate::{ManualClock, Registry, ScriptedAgent, Session};
use std::sync::Arc;

fn base(name: &str) -> SessionSummary {
    let registry = Registry::new();
    registry
        .register(Session::new(
            name,
            Box::new(ScriptedAgent::new(vec![])),
            Arc::new(ManualClock::new()),
        ))
        .unwrap();
    registry.list().remove(0)
}
fn empty_facts() -> PresentationFacts {
    PresentationFacts {
        kind: FieldState::Unavailable,
        model: FieldState::Unavailable,
        context_used: FieldState::Unavailable,
        context_window: FieldState::Unavailable,
        context_percent: FieldState::Unavailable,
    }
}
fn expected(token: u32, parent: Option<u32>, depth: usize) -> ResolvedSession {
    ResolvedSession {
        token,
        parent_token: parent,
        parent_label: parent.map(|p| format!("wire-{}", p - 1)),
        depth,
        metadata: MetadataState::Available(empty_facts()),
        program_status: None,
    }
}
fn forest(edges: &[(&str, Option<&str>)]) -> Result<Vec<ResolvedSession>, EnrichmentError> {
    let summaries: Vec<_> = edges
        .iter()
        .enumerate()
        .map(|(i, _)| base(&format!("wire-{i}")))
        .collect();
    let facts: Vec<_> = edges
        .iter()
        .map(|(member, parent)| ButlerFacts {
            member_id: (*member).into(),
            leader_id: parent.map(String::from),
            alias: Some("same-label".into()),
            kind: FieldState::Unavailable,
            model: FieldState::Unavailable,
            context_used: FieldState::Unavailable,
            context_window: FieldState::Unavailable,
            context_percent: FieldState::Unavailable,
        })
        .collect();
    let targets: Vec<_> = summaries
        .iter()
        .zip(&facts)
        .enumerate()
        .map(|(i, (summary, facts))| ListedTarget {
            token: i as u32 + 1,
            summary,
            managed: true,
            butler: Some(facts),
            facts_remaining_ms: 10000,
        })
        .collect();
    resolve_cluster_roster(&NodeRoster {
        stale_or_unreachable: false,
        list_elapsed_ms: 0,
        targets: &targets,
    })
}
#[test]
fn enrichment_contract_callable_preorder_uses_ids_not_labels() {
    assert_eq!(
        forest(&[("G", Some("C")), ("S", None), ("L", None), ("C", Some("L"))]),
        Ok(vec![
            expected(2, None, 0),
            expected(3, None, 0),
            expected(4, Some(3), 1),
            expected(1, Some(4), 2)
        ])
    );
}

#[test]
fn enrichment_contract_no_sidecar_request_before_positive_v1_capability() {
    let old = vec!["v2".to_owned()];
    let supported = vec!["v1".to_owned()];
    assert_eq!(
        vec![
            plan_cluster_facts_request(None),
            plan_cluster_facts_request(Some(&old)),
            plan_cluster_facts_request(Some(&supported))
        ],
        vec![
            Ok(ClusterFactsRequestPlan::BaseListOnly),
            Ok(ClusterFactsRequestPlan::BaseListOnly),
            Ok(ClusterFactsRequestPlan::FetchFactsV1)
        ]
    );
}

#[test]
fn enrichment_contract_same_three_level_relations_as_local_hook_with_cluster_margins() {
    // Same root->child->leaf fixture as Butler's current-main local control.
    // Its visual margins are 0/0/1; these cluster depths are 0/1/2.
    assert_eq!(
        forest(&[
            ("root", None),
            ("child", Some("root")),
            ("leaf", Some("child"))
        ]),
        Ok(vec![
            expected(1, None, 0),
            expected(2, Some(1), 1),
            expected(3, Some(2), 2)
        ])
    );
}
#[test]
fn enrichment_contract_orphan_and_self_edges_are_roots() {
    assert_eq!(
        forest(&[("A", Some("missing")), ("B", Some("B"))]),
        Ok(vec![expected(1, None, 0), expected(2, None, 0)])
    );
}
#[test]
fn enrichment_contract_duplicate_member_ids_do_not_choose_a_parent() {
    assert_eq!(
        forest(&[("A", None), ("A", None), ("B", Some("A"))]),
        Ok(vec![
            expected(1, None, 0),
            expected(2, None, 0),
            expected(3, None, 0)
        ])
    );
}
#[test]
fn enrichment_contract_three_cycle_removes_internal_edges_but_keeps_valid_children() {
    assert_eq!(
        forest(&[
            ("A", Some("B")),
            ("B", Some("C")),
            ("C", Some("A")),
            ("D", Some("B"))
        ]),
        Ok(vec![
            expected(1, None, 0),
            expected(2, None, 0),
            expected(4, Some(2), 1),
            expected(3, None, 0)
        ])
    );
}
#[test]
fn enrichment_contract_parent_alias_collision_never_joins() {
    assert_eq!(
        forest(&[("A", None), ("B", Some("same-label"))]),
        Ok(vec![expected(1, None, 0), expected(2, None, 0)])
    );
}
#[test]
fn enrichment_contract_same_ids_in_different_node_rosters_do_not_join() {
    let results = vec![forest(&[("A", Some("B"))]), forest(&[("B", Some("A"))])];
    assert_eq!(
        results,
        vec![
            Ok(vec![expected(1, None, 0)]),
            Ok(vec![expected(1, None, 0)])
        ]
    );
}
#[test]
fn enrichment_contract_5000_chain_has_exact_logical_depth_without_recursion() {
    let names: Vec<_> = (0..5000).map(|i| format!("member-{i}")).collect();
    let edges: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i.checked_sub(1).map(|p| names[p].as_str())))
        .collect();
    let expected_rows = (0..5000)
        .map(|i| expected(i + 1, if i == 0 { None } else { Some(i) }, i as usize))
        .collect();
    assert_eq!(forest(&edges), Ok(expected_rows));
}
#[test]
fn enrichment_contract_unknown_remote_without_facts_returns_identity_rows_not_error() {
    let summaries = [base("ordinary-a"), base("ordinary-b")];
    let targets = [
        ListedTarget {
            token: 9,
            summary: &summaries[0],
            managed: false,
            butler: None,
            facts_remaining_ms: 0,
        },
        ListedTarget {
            token: 3,
            summary: &summaries[1],
            managed: false,
            butler: None,
            facts_remaining_ms: 0,
        },
    ];
    let result = resolve_cluster_roster(&NodeRoster {
        stale_or_unreachable: false,
        list_elapsed_ms: 0,
        targets: &targets,
    });
    let rows = [9, 3]
        .into_iter()
        .map(|token| ResolvedSession {
            token,
            parent_token: None,
            parent_label: None,
            depth: 0,
            metadata: MetadataState::NotProvided,
            program_status: None,
        })
        .collect();
    assert_eq!(result, Ok(rows));
}
#[test]
fn enrichment_contract_late_arrival_and_replay_do_not_renew_managed_facts() {
    let summary = base("worker");
    let facts = ButlerFacts {
        member_id: "C".into(),
        leader_id: None,
        alias: None,
        kind: FieldState::Value("codex".into()),
        model: FieldState::Value("known".into()),
        context_used: FieldState::Value(42),
        context_window: FieldState::Value(100),
        context_percent: FieldState::Value(42),
    };
    let targets = [ListedTarget {
        token: 1,
        summary: &summary,
        managed: true,
        butler: Some(&facts),
        facts_remaining_ms: 1000,
    }];
    let roster = NodeRoster {
        stale_or_unreachable: false,
        list_elapsed_ms: 1100,
        targets: &targets,
    };
    let expected_rows = vec![ResolvedSession {
        token: 1,
        parent_token: None,
        parent_label: None,
        depth: 0,
        metadata: MetadataState::Unavailable,
        program_status: None,
    }];
    assert_eq!(
        vec![
            resolve_cluster_roster(&roster),
            resolve_cluster_roster(&roster)
        ],
        vec![Ok(expected_rows.clone()), Ok(expected_rows)]
    );
}
#[test]
fn enrichment_contract_program_status_is_authoritative_for_any_live_instance() {
    let actual: Vec<_> = [
        ProgramState::Working,
        ProgramState::Idle,
        ProgramState::Blocked,
        ProgramState::Done,
        ProgramState::Error,
    ]
    .into_iter()
    .map(|state| {
        let mut summary = base("ordinary-shell");
        summary.program_status = Some(ProgramStatus {
            state,
            app: None,
            kind: Some(ProgramBlockKind::Permission),
            progress: None,
        });
        summary.program_status_age_ms = Some(0);
        let targets = [ListedTarget {
            token: 1,
            summary: &summary,
            managed: false,
            butler: None,
            facts_remaining_ms: 0,
        }];
        let expected_rows = vec![ResolvedSession {
            token: 1,
            parent_token: None,
            parent_label: None,
            depth: 0,
            metadata: MetadataState::NotProvided,
            program_status: summary.program_status.clone(),
        }];
        (
            resolve_cluster_roster(&NodeRoster {
                stale_or_unreachable: false,
                list_elapsed_ms: 0,
                targets: &targets,
            }),
            Ok(expected_rows),
        )
    })
    .collect();
    let (results, expected_rows): (Vec<_>, Vec<_>) = actual.into_iter().unzip();
    assert_eq!(results, expected_rows);
}
#[test]
fn enrichment_contract_ended_unknown_stale_missing_age_or_instance_suppress_advice() {
    let mut results = Vec::new();
    for case in 0..6 {
        let mut summary = base("shell");
        summary.program_status = Some(ProgramStatus {
            state: ProgramState::Working,
            app: None,
            kind: None,
            progress: None,
        });
        summary.program_status_age_ms = Some(0);
        match case {
            0 => summary.alive = false,
            1 => summary.unavailable = true,
            2 => summary.program_status_age_ms = None,
            3 => summary.program_status_age_ms = Some(600001),
            4 => summary.instance_id = None,
            _ => summary.program_status.as_mut().unwrap().state = ProgramState::Unknown,
        }
        let targets = [ListedTarget {
            token: 1,
            summary: &summary,
            managed: false,
            butler: None,
            facts_remaining_ms: 0,
        }];
        results.push(resolve_cluster_roster(&NodeRoster {
            stale_or_unreachable: case == 1,
            list_elapsed_ms: 0,
            targets: &targets,
        }));
    }
    let row = ResolvedSession {
        token: 1,
        parent_token: None,
        parent_label: None,
        depth: 0,
        metadata: MetadataState::NotProvided,
        program_status: None,
    };
    assert_eq!(results, vec![Ok(vec![row]); 6]);
}
#[test]
fn enrichment_contract_formatter_exact_wide_depth_cap_and_narrow_state_priority() {
    let row = ResolvedSession {
        token: 1,
        parent_token: None,
        parent_label: None,
        depth: 4999,
        metadata: MetadataState::Available(PresentationFacts {
            kind: FieldState::Value("codex".into()),
            model: FieldState::Value("model".into()),
            context_used: FieldState::Value(42000),
            context_window: FieldState::Value(100000),
            context_percent: FieldState::Value(42),
        }),
        program_status: None,
    };
    let full = format!(
        "{}worker · codex · model · 42K/100K · 42% ctx · unreachable",
        " ".repeat(40)
    );
    assert_eq!(
        format_cluster_row(&row, "worker", "unreachable", 240),
        Ok(full.clone())
    );
    let results: Vec<_> = [40, 60, 80, 120]
        .into_iter()
        .map(|width| format_cluster_row(&row, "worker", "unreachable", width))
        .collect();
    assert_eq!(
        results,
        vec![
            Ok("worker · unreachable".into()),
            Ok("worker · unreachable".into()),
            Ok("worker · unreachable".into()),
            Ok(full)
        ]
    );
}
#[test]
fn enrichment_contract_formatter_hides_unknown_metadata_and_private_join_keys() {
    let row = ResolvedSession {
        token: 17,
        parent_token: Some(11),
        parent_label: Some("human leader".into()),
        depth: 1,
        metadata: MetadataState::NotProvided,
        program_status: None,
    };
    assert_eq!(
        format_cluster_row(&row, "worker", "live", 120),
        Ok("  worker · ← human leader · live".into())
    );
    let unknown = ResolvedSession {
        parent_token: None,
        parent_label: None,
        depth: 0,
        ..row
    };
    assert_eq!(
        format_cluster_row(&unknown, "worker", "live", 120),
        Ok("worker · live".into())
    );
}

#[test]
fn enrichment_contract_managed_missing_fields_have_field_level_markers_only() {
    let row = ResolvedSession {
        token: 1,
        parent_token: None,
        parent_label: None,
        depth: 0,
        metadata: MetadataState::Available(PresentationFacts {
            kind: FieldState::Value("codex".into()),
            model: FieldState::Unavailable,
            context_used: FieldState::Unavailable,
            context_window: FieldState::NotApplicable,
            context_percent: FieldState::Unavailable,
        }),
        program_status: None,
    };
    assert_eq!(
        format_cluster_row(&row, "worker", "live", 240),
        Ok("worker · codex · model unavailable · context unavailable · live".into())
    );
}

#[test]
fn enrichment_contract_zero_usage_is_a_proven_value_not_unavailable() {
    let row = ResolvedSession {
        token: 1,
        parent_token: None,
        parent_label: None,
        depth: 0,
        metadata: MetadataState::Available(PresentationFacts {
            kind: FieldState::NotApplicable,
            model: FieldState::NotApplicable,
            context_used: FieldState::Value(0),
            context_window: FieldState::Value(100000),
            context_percent: FieldState::Value(0),
        }),
        program_status: None,
    };
    assert_eq!(
        format_cluster_row(&row, "worker", "live", 240),
        Ok("worker · 0K/100K · 0% ctx · live".into())
    );
}

fn assert_rendered_rows_hide_private_join_ids(actual: &Result<Vec<String>, EnrichmentError>) {
    if let Ok(rows) = actual {
        for row in rows {
            assert!(
                !row.contains("private-MID")
                    && !row.contains("member_id")
                    && !row.contains("leader_id")
            );
        }
    }
}

#[test]
fn enrichment_contract_internal_join_ids_never_reach_rendered_rows() {
    let summaries = [base("human leader"), base("worker")];
    let facts = [
        ButlerFacts {
            member_id: "private-MID-lead".into(),
            leader_id: None,
            alias: None,
            kind: FieldState::NotApplicable,
            model: FieldState::NotApplicable,
            context_used: FieldState::NotApplicable,
            context_window: FieldState::NotApplicable,
            context_percent: FieldState::NotApplicable,
        },
        ButlerFacts {
            member_id: "private-MID-child".into(),
            leader_id: Some("private-MID-lead".into()),
            alias: None,
            kind: FieldState::NotApplicable,
            model: FieldState::NotApplicable,
            context_used: FieldState::NotApplicable,
            context_window: FieldState::NotApplicable,
            context_percent: FieldState::NotApplicable,
        },
    ];
    let targets: Vec<_> = summaries
        .iter()
        .zip(&facts)
        .enumerate()
        .map(|(i, (summary, facts))| ListedTarget {
            token: i as u32 + 1,
            summary,
            managed: true,
            butler: Some(facts),
            facts_remaining_ms: 10000,
        })
        .collect();
    let actual = resolve_cluster_roster(&NodeRoster {
        stale_or_unreachable: false,
        list_elapsed_ms: 0,
        targets: &targets,
    })
    .and_then(|rows| {
        rows.iter()
            .enumerate()
            .map(|(i, row)| format_cluster_row(row, &summaries[i].name, "live", 240))
            .collect()
    });
    assert_eq!(
        actual,
        Ok(vec![
            "human leader · live".into(),
            "  worker · ← human leader · live".into()
        ])
    );
    assert_rendered_rows_hide_private_join_ids(&actual);
}
