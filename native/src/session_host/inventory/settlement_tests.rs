use super::tests::reserved;
use super::*;
use crate::session_host::inventory::model_tests::{
    apply, completion, identity, key, reserve, target,
};

fn terminal(index: &mut Index, lease: &Lease, kind: TerminalEnd) {
    for publication in [
        Publication::Exited {
            generation: 41,
            code: Some(0),
            signal: None,
        },
        Publication::TerminalEnd {
            generation: 42,
            kind,
        },
    ] {
        *index = index.publish(index.cursor(), lease, publication).unwrap();
    }
}
fn ready() -> (Index, Fence, Lease) {
    let (index, fence, lease) = reserved();
    let index = index
        .publish(index.cursor(), &lease, Publication::Ready)
        .unwrap();
    (index, fence, lease)
}
fn settled() -> (Index, Fence, Lease) {
    let (mut index, fence, lease) = ready();
    terminal(&mut index, &lease, TerminalEnd::Eof);
    (index, fence, lease)
}
fn retire(index: &mut Index, fence: Fence) {
    apply(
        index,
        fence,
        Mutation::Exit {
            target: target(),
            operation: ExitMutation::Retire,
        },
    );
}

#[test]
fn proof_needs_one_valid_terminal_cut_not_empty_retention_or_exit_alone() {
    let (mut index, _, lease) = reserved();
    assert_eq!(lease.has_unconsumed(&index), Ok(false));
    assert!(lease.settled(&index).is_err());
    index = index
        .publish(index.cursor(), &lease, Publication::Ready)
        .unwrap();
    assert!(lease.settled(&index).is_err());
    index = index
        .publish(
            index.cursor(),
            &lease,
            Publication::Exited {
                generation: 41,
                code: Some(0),
                signal: None,
            },
        )
        .unwrap();
    assert!(lease.settled(&index).is_err());
    for kind in [
        TerminalEnd::Eof,
        TerminalEnd::Hangup,
        TerminalEnd::ReadFailure,
    ] {
        let end = index
            .publish(
                index.cursor(),
                &lease,
                Publication::TerminalEnd {
                    generation: 42,
                    kind,
                },
            )
            .unwrap();
        assert_eq!(
            lease.settled(&end).is_ok(),
            kind != TerminalEnd::ReadFailure
        );
    }
}

#[test]
fn pending_at_exit_and_later_completion_refuse_until_exact_durable_consumption() {
    let (mut index, fence, lease) = ready();
    apply(&mut index, fence, reserve(1, 6));
    terminal(&mut index, &lease, TerminalEnd::Eof);
    assert!(matches!(lease.settled(&index), Err(Error::Busy)));
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Complete {
                key: key(1, 6),
                completion: completion(),
            },
        },
    );
    assert!(matches!(lease.settled(&index), Err(Error::Busy)));
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Consume {
                key: key(1, 6),
                checkpoint: ConsumerCheckpoint {
                    intent: [6; 16],
                    outcome: hash(&serde_json::to_vec(&completion()).unwrap()),
                    metadata: MetadataUpdate {
                        namespace: "consumer".into(),
                        expected: None,
                        value: Namespace::new(1, "{}").unwrap(),
                    },
                },
            },
        },
    );
    let proof = lease.settled(&index).unwrap();
    assert!(proof.confirms(&lease, &identity()));
    assert_eq!(proof.exit_generation(), 41);
}

#[test]
fn every_lease_binding_and_actual_identity_is_exact_and_nontransferable() {
    let (index, _, lease) = settled();
    let proof = lease.settled(&index).unwrap();
    let mut variants = Vec::new();
    let mut wrong = lease.clone();
    wrong.store = [8; 16];
    variants.push(wrong);
    let mut wrong = lease.clone();
    wrong.name = "other".into();
    variants.push(wrong);
    let mut wrong = lease.clone();
    wrong.identity.host = [8; 16];
    variants.push(wrong);
    let mut wrong = lease.clone();
    wrong.identity.session = "other".into();
    variants.push(wrong);
    let mut wrong = lease.clone();
    wrong.identity.instance = "other".into();
    variants.push(wrong);
    let mut wrong = lease.clone();
    wrong.launch = [8; 16];
    variants.push(wrong);
    let mut wrong = lease.clone();
    wrong.nonce = [8; 16];
    variants.push(wrong);
    for wrong in variants {
        assert!(wrong.settled(&index).is_err());
        assert!(!proof.confirms(&wrong, &identity()));
    }
    let mut wrong = identity();
    wrong.instance = "replacement".into();
    assert!(!proof.confirms(&lease, &wrong));
    assert!(lease.settled(&Index::new([9; 16])).is_err());
}

#[test]
fn corrupted_or_future_required_cut_cannot_construct_evidence() {
    let (index, _, lease) = settled();
    let mut bad = index.clone();
    bad.entries.get_mut("synthetic").unwrap().phase = Phase::Exited { generation: 40 };
    assert!(lease.settled(&bad).is_err());
    let mut bad = index.clone();
    bad.required.push("future-stop-semantics-v2".into());
    assert!(matches!(lease.settled(&bad), Err(Error::Incompatible)));
    let mut bad = index;
    bad.allocations.clear();
    assert!(lease.settled(&bad).is_err());
}

#[test]
fn valid_mutations_cannot_resurrect_retention_and_unrelated_revisions_do_not_revoke_proof() {
    let (mut index, fence, lease) = settled();
    let proof = lease.settled(&index).unwrap();
    let before = index.encode().unwrap();
    for mutation in [
        reserve(1, 6),
        Mutation::Register(target()),
        Mutation::Input {
            target: target(),
            operation: InputMutation::Adopt(key(1, 6)),
        },
        Mutation::Input {
            target: target(),
            operation: InputMutation::Observe {
                key: key(1, 6),
                phase: InputPhase::Admitted,
            },
        },
        Mutation::Input {
            target: target(),
            operation: InputMutation::Complete {
                key: key(1, 6),
                completion: completion(),
            },
        },
    ] {
        assert!(index.apply(index.cursor(), Some(fence), mutation).is_err());
    }
    assert!(index
        .publish(index.cursor(), &lease, Publication::Ready)
        .is_err());
    assert_eq!(index.encode().unwrap(), before);
    apply(
        &mut index,
        fence,
        Mutation::Metadata {
            target: target(),
            update: MetadataUpdate {
                namespace: "unrelated".into(),
                expected: None,
                value: Namespace::new(1, "{}").unwrap(),
            },
        },
    );
    index = index
        .apply(index.cursor(), None, Mutation::Claim { nonce: [9; 16] })
        .unwrap()
        .next;
    assert!(proof.confirms(&lease, &identity()));
    assert!(lease.settled(&index).is_ok());
}

#[test]
fn retire_before_proof_refuses_and_tombstone_wrap_never_recycles_old_identity() {
    let (mut index, fence, lease) = settled();
    let proof = lease.settled(&index).unwrap();
    retire(&mut index, fence);
    assert!(lease.settled(&index).is_err());
    for number in 0..=MAX_TOMBSTONES {
        let mut id = identity();
        id.instance = format!("replacement-{number}");
        apply(
            &mut index,
            fence,
            Mutation::ReserveLaunch {
                name: "synthetic".into(),
                entry: Entry::reserved(id.clone(), [4; 16], "{}").unwrap(),
            },
        );
        let replacement = || Target {
            name: "synthetic".into(),
            identity: id.clone(),
        };
        apply(&mut index, fence, Mutation::Register(replacement()));
        apply(
            &mut index,
            fence,
            Mutation::Exit {
                target: replacement(),
                operation: ExitMutation::Observe { generation: 1 },
            },
        );
        apply(
            &mut index,
            fence,
            Mutation::Exit {
                target: replacement(),
                operation: ExitMutation::Retire,
            },
        );
    }
    assert!(!index.tombstones.iter().any(|t| t.identity == identity()));
    assert!(index
        .apply(
            index.cursor(),
            Some(fence),
            Mutation::ReserveLaunch {
                name: "synthetic".into(),
                entry: Entry::reserved(identity(), [4; 16], "{}").unwrap(),
            }
        )
        .is_err());
    assert!(lease.settled(&index).is_err());
    assert!(proof.confirms(&lease, &identity()));
}
