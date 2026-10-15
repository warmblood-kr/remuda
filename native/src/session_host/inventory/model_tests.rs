use super::super::api::Identity;
use super::super::result::{Completion, Delivery, Reply};
use super::super::wire::InputAct;
use super::model::*;

pub(super) fn identity() -> Identity {
    Identity {
        host: [3; 16],
        session: "stored-session".into(),
        instance: "stored-instance".into(),
    }
}
pub(super) fn target() -> Target {
    Target {
        name: "synthetic".into(),
        identity: identity(),
    }
}
pub(super) fn initial() -> (Index, Fence) {
    let index = Index::new([1; 16]);
    let applied = index
        .apply(index.cursor(), None, Mutation::Claim { nonce: [2; 16] })
        .unwrap();
    (applied.next, applied.fence)
}
pub(super) fn apply(index: &mut Index, fence: Fence, mutation: Mutation) {
    *index = index
        .apply(index.cursor(), Some(fence), mutation)
        .unwrap()
        .next;
}
pub(super) fn registered(index: &mut Index, fence: Fence) {
    apply(
        index,
        fence,
        Mutation::ReserveLaunch {
            name: "synthetic".into(),
            entry: Entry::reserved(
                identity(),
                [4; 16],
                r#"{"argv":["synthetic"],"cwd":"/private/synthetic"}"#,
            )
            .unwrap(),
        },
    );
    apply(index, fence, Mutation::Register(target()));
}
pub(super) fn key(sequence: u64, client: u8) -> Key {
    Key {
        client: [client; 16],
        sequence,
        payload: act().digest().unwrap(),
    }
}
pub(super) fn act() -> InputAct {
    InputAct::InputText("original private payload".into())
}
pub(super) fn reserve(sequence: u64, client: u8) -> Mutation {
    Mutation::Input {
        target: target(),
        operation: InputMutation::Reserve {
            intent: [client; 16],
            key: key(sequence, client),
            act: act(),
        },
    }
}
pub(super) fn completion() -> Completion {
    Completion {
        delivery: Delivery::Written,
        reply: Reply::Unit,
    }
}
#[test]
fn corrupt_event_identity_cannot_hide_oversized_tail_behind_progress_credit() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    index
        .events
        .back_mut()
        .unwrap()
        .identity
        .as_mut()
        .unwrap()
        .instance = "x".repeat(4096);
    let bytes = index.encode().unwrap();
    assert!(bytes.len() < MAX_BYTES);
    assert!(matches!(Index::decode(&bytes), Err(Error::Corrupt)));
}
fn complete(sequence: u64, client: u8) -> Mutation {
    Mutation::Input {
        target: target(),
        operation: InputMutation::Complete {
            key: key(sequence, client),
            completion: completion(),
        },
    }
}
fn consume(sequence: u64, client: u8, expected: Option<u64>) -> Mutation {
    Mutation::Input {
        target: target(),
        operation: InputMutation::Consume {
            key: key(sequence, client),
            checkpoint: ConsumerCheckpoint {
                intent: [client; 16],
                outcome: hash(&serde_json::to_vec(&completion()).unwrap()),
                metadata: MetadataUpdate {
                    namespace: "consumer".into(),
                    expected,
                    value: Namespace::new(1, r#"{"consumer":"durable-checkpoint"}"#).unwrap(),
                },
            },
        },
    }
}
#[test]
fn retired_identity_never_recycles_even_after_tombstone_wrap_and_lifetime_exhaustion() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    for number in 0..1024 {
        let id = if number == 0 {
            identity()
        } else {
            Identity {
                host: [3; 16],
                session: format!("session-{number}"),
                instance: format!("instance-{number}"),
            }
        };
        let target = || Target {
            name: "synthetic".into(),
            identity: id.clone(),
        };
        if number != 0 {
            apply(
                &mut index,
                fence,
                Mutation::ReserveLaunch {
                    name: "synthetic".into(),
                    entry: Entry::reserved(id.clone(), [4; 16], "{}").unwrap(),
                },
            );
            apply(&mut index, fence, Mutation::Register(target()));
        }
        if number == 1023 {
            break;
        }
        apply(
            &mut index,
            fence,
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Observe { generation: 1 },
            },
        );
        apply(
            &mut index,
            fence,
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Retire,
            },
        );
    }
    assert_eq!(index.tombstones.len(), 128);
    let mut index = Index::decode(&index.encode().unwrap()).unwrap();
    let before = index.cursor();
    assert!(matches!(
        index.apply(
            before,
            Some(fence),
            Mutation::ReserveLaunch {
                name: "reuse".into(),
                entry: Entry::reserved(identity(), [4; 16], "{}").unwrap()
            }
        ),
        Err(Error::Conflict)
    ));
    let new = Identity {
        host: [99; 16],
        session: "new".into(),
        instance: "new".into(),
    };
    assert!(matches!(
        index.apply(
            before,
            Some(fence),
            Mutation::ReserveLaunch {
                name: "new".into(),
                entry: Entry::reserved(new, [4; 16], "{}").unwrap()
            }
        ),
        Err(Error::Exhausted)
    ));
    assert_eq!(index.cursor(), before);
    existing_work_at_exhaustion(&mut index, fence);
}
fn existing_work_at_exhaustion(index: &mut Index, fence: Fence) {
    let identity = index.entries["synthetic"].identity.clone();
    for mutation in [reserve(1, 7), complete(1, 7), consume(1, 7, None)] {
        let Mutation::Input { operation, .. } = mutation else {
            unreachable!()
        };
        apply(
            index,
            fence,
            Mutation::Input {
                target: Target {
                    name: "synthetic".into(),
                    identity: identity.clone(),
                },
                operation,
            },
        );
    }
    let index = Index::decode(&index.encode().unwrap()).unwrap();
    assert!(index.entries["synthetic"].pending.is_none());
    assert!(index.entries["synthetic"].streams[0].results.is_empty());
}
fn fill_metadata(mut index: Index, fence: Fence, payload: &str) -> Index {
    let mut filled = false;
    'fill: for number in 0..40 {
        let name = format!("fill-{number}");
        let id = Identity {
            host: [8; 16],
            session: name.clone(),
            instance: name.clone(),
        };
        apply(
            &mut index,
            fence,
            Mutation::ReserveLaunch {
                name: name.clone(),
                entry: Entry::reserved(id.clone(), [4; 16], "{}").unwrap(),
            },
        );
        for slot in 0..15 {
            let mutation = Mutation::Metadata {
                target: Target {
                    name: name.clone(),
                    identity: id.clone(),
                },
                update: MetadataUpdate {
                    namespace: format!("slot-{slot}"),
                    expected: None,
                    value: Namespace::new(1, payload).unwrap(),
                },
            };
            match index.apply(index.cursor(), Some(fence), mutation) {
                Ok(applied) => index = applied.next,
                Err(Error::Capacity) => {
                    filled = true;
                    break 'fill;
                }
                _ => panic!("unexpected fill result"),
            }
        }
    }
    assert!(filled && index.encode().unwrap().len() > 3_000_000);
    index
}
fn fill_ordinary_slots(index: &mut Index, fence: Fence, payload: &str) {
    for slot in 0..15 {
        apply(
            index,
            fence,
            Mutation::Metadata {
                target: target(),
                update: MetadataUpdate {
                    namespace: format!("slot-{slot}"),
                    expected: None,
                    value: Namespace::new(1, payload).unwrap(),
                },
            },
        );
    }
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            Mutation::Metadata {
                target: target(),
                update: MetadataUpdate {
                    namespace: "no-consumer-slot".into(),
                    expected: None,
                    value: Namespace::new(1, "{}").unwrap(),
                }
            }
        ),
        Err(Error::Capacity)
    ));
}
#[test]
fn projected_capacity_covers_multiple_pending_and_out_of_order_retained_acks() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    let second = || Target {
        name: "second".into(),
        identity: Identity {
            host: [8; 16],
            session: "second-session".into(),
            instance: "second-instance".into(),
        },
    };
    apply(
        &mut index,
        fence,
        Mutation::ReserveLaunch {
            name: "second".into(),
            entry: Entry::reserved(second().identity, [5; 16], "{}").unwrap(),
        },
    );
    apply(&mut index, fence, Mutation::Register(second()));
    for sequence in 1..=2 {
        apply(&mut index, fence, reserve(sequence, 7));
        apply(&mut index, fence, complete(sequence, 7));
    }
    apply(&mut index, fence, reserve(3, 7));
    let Mutation::Input { operation, .. } = reserve(1, 7) else {
        unreachable!()
    };
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: second(),
            operation,
        },
    );
    let payload = format!("\"{}\"", "x".repeat(MAX_METADATA - 2));
    index = fill_metadata(index, fence, &payload);
    let mut corrupt = index.clone();
    for slot in 0..2 {
        corrupt.entries.get_mut("second").unwrap().metadata.insert(
            format!("over-budget-{slot}"),
            Namespace::new(1, &payload).unwrap(),
        );
    }
    let encoded = corrupt.encode().unwrap();
    assert!(encoded.len() < MAX_BYTES);
    assert!(matches!(Index::decode(&encoded), Err(Error::Capacity)));
    for (target, sequence) in [(target(), 3), (second(), 1)] {
        let Mutation::Input { operation, .. } = complete(sequence, 7) else {
            unreachable!()
        };
        apply(&mut index, fence, Mutation::Input { target, operation });
    }
    for (target, sequence, expected) in [
        (target(), 2, None),
        (target(), 1, Some(1)),
        (target(), 3, Some(2)),
        (second(), 1, None),
    ] {
        let Mutation::Input { operation, .. } = consume(sequence, 7, expected) else {
            unreachable!()
        };
        apply(&mut index, fence, Mutation::Input { target, operation });
        index = Index::decode(&index.encode().unwrap()).unwrap();
        if sequence == 2 {
            let stream = &index.entries["synthetic"].streams[0];
            assert_eq!(stream.results.len(), 3);
            assert!(stream.results[1].consumer_ack);
            assert!(!stream.results[0].consumer_ack);
        }
    }
    for name in ["synthetic", "second"] {
        assert!(index.entries[name].streams[0].results.is_empty());
        assert!(index.entries[name].pending.is_none());
    }
}
#[test]
fn aggregate_admission_reserves_completion_ack_and_progress_space_against_metadata_fill() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    let payload = format!("\"{}\"", "x".repeat(MAX_METADATA - 2));
    fill_ordinary_slots(&mut index, fence, &payload);
    index = fill_metadata(index, fence, &payload);
    for _ in 0..300 {
        apply(
            &mut index,
            fence,
            Mutation::Input {
                target: target(),
                operation: InputMutation::Adopt(key(1, 7)),
            },
        );
    }
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Observe {
                key: key(1, 7),
                phase: InputPhase::Indeterminate,
            },
        },
    );
    let completion = Completion {
        delivery: Delivery::Written,
        reply: Reply::Cleared(Some("r".repeat(4000))),
    };
    assert!(completion.fits());
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Complete {
                key: key(1, 7),
                completion: completion.clone(),
            },
        },
    );
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Consume {
                key: key(1, 7),
                checkpoint: ConsumerCheckpoint {
                    intent: [7; 16],
                    outcome: hash(&serde_json::to_vec(&completion).unwrap()),
                    metadata: MetadataUpdate {
                        namespace: "consumer".into(),
                        expected: None,
                        value: Namespace::new(1, &payload).unwrap(),
                    },
                },
            },
        },
    );
    let recovered = Index::decode(&index.encode().unwrap()).unwrap();
    assert!(matches!(
        recovered.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Forgotten
    ));
    assert_eq!(
        recovered.entries["synthetic"].metadata["consumer"]
            .payload
            .get(),
        payload
    );
}
#[test]
fn independent_writer_fence_and_revision_refuse_stale_or_foreign_writes() {
    let (mut index, old) = initial();
    let before = index.cursor();
    let next = index
        .apply(before, None, Mutation::Claim { nonce: [5; 16] })
        .unwrap();
    index = next.next;
    assert!(matches!(
        index.apply(index.cursor(), Some(old), Mutation::Register(target())),
        Err(Error::StaleFence)
    ));
    assert!(matches!(
        index.apply(before, Some(next.fence), Mutation::Register(target())),
        Err(Error::StaleRevision)
    ));
    assert!(matches!(
        index.apply(
            Revision {
                store: [9; 16],
                number: index.cursor().number
            },
            None,
            Mutation::Claim { nonce: [6; 16] }
        ),
        Err(Error::StaleStore)
    ));
}
#[test]
fn reservation_keeps_original_payload_across_new_owner_and_lookup_does_not_admit() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    let next = index
        .apply(index.cursor(), None, Mutation::Claim { nonce: [8; 16] })
        .unwrap();
    index = next.next;
    assert!(matches!(
        index.apply(index.cursor(), Some(next.fence), complete(1, 7)),
        Err(Error::StaleFence)
    ));
    apply(
        &mut index,
        next.fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Adopt(key(1, 7)),
        },
    );
    let persisted = Index::decode(&index.encode().unwrap()).unwrap();
    let Lookup::Pending(pending) = persisted.lookup(&target(), &key(1, 7)).unwrap() else {
        panic!("not retained")
    };
    assert_eq!(pending.intent, [7; 16]);
    assert_eq!(pending.store, index.cursor().store);
    assert_eq!(pending.writer_epoch, 2);
    assert!(matches!(&pending.act,InputAct::InputText(text) if text=="original private payload"));
    assert_eq!(pending.phase, InputPhase::Unresolved);
}
#[test]
fn one_unresolved_scheduler_rejects_other_client_and_changed_payload_without_changes() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    let bytes = index.encode().unwrap();
    assert!(matches!(
        index.apply(index.cursor(), Some(fence), reserve(1, 8)),
        Err(Error::Busy)
    ));
    let mut changed = key(1, 7);
    changed.payload[0] ^= 1;
    assert!(matches!(
        index.lookup(&target(), &changed).unwrap(),
        Lookup::Conflict
    ));
    assert_eq!(bytes, index.encode().unwrap());
}
#[test]
fn result_and_payload_remain_until_consumer_checkpoint_commits_atomically() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    apply(&mut index, fence, complete(1, 7));
    let Lookup::Complete(result) = index.lookup(&target(), &key(1, 7)).unwrap() else {
        panic!("missing result")
    };
    assert!(
        matches!(&result.reservation.act,InputAct::InputText(text) if text=="original private payload")
    );
    assert!(!result.consumer_ack);
    let mut bad = match consume(1, 7, None) {
        Mutation::Input {
            operation: InputMutation::Consume { checkpoint, .. },
            ..
        } => checkpoint,
        _ => unreachable!(),
    };
    bad.outcome[0] ^= 1;
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            Mutation::Input {
                target: target(),
                operation: InputMutation::Consume {
                    key: key(1, 7),
                    checkpoint: bad
                }
            }
        ),
        Err(Error::Conflict)
    ));
    assert!(!index.entries["synthetic"].metadata.contains_key("consumer"));
    apply(&mut index, fence, consume(1, 7, None));
    assert!(matches!(
        index.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Forgotten
    ));
    assert_eq!(index.entries["synthetic"].metadata["consumer"].revision, 1);
    assert!(matches!(
        index.apply(index.cursor(), Some(fence), reserve(1, 7)),
        Err(Error::Conflict)
    ));
}
#[test]
fn unconsumed_capacity_refuses_new_input_instead_of_evicting_results() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    for seq in 1..=MAX_RESULTS as u64 {
        apply(&mut index, fence, reserve(seq, 7));
        apply(&mut index, fence, complete(seq, 7));
    }
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            reserve(MAX_RESULTS as u64 + 1, 7)
        ),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        index.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Complete(_)
    ));
}
#[test]
fn consumed_clients_are_never_recycled_to_allow_old_sequence_replay() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    for client in 0..MAX_CLIENTS as u8 {
        apply(&mut index, fence, reserve(1, client));
        apply(&mut index, fence, complete(1, client));
        apply(
            &mut index,
            fence,
            consume(
                1,
                client,
                if client == 0 {
                    None
                } else {
                    Some(u64::from(client))
                },
            ),
        );
    }
    assert_eq!(index.entries["synthetic"].streams.len(), MAX_CLIENTS);
    assert!(matches!(
        index.apply(index.cursor(), Some(fence), reserve(1, 99)),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        index.lookup(&target(), &key(1, 0)).unwrap(),
        Lookup::Forgotten
    ));
}
#[test]
fn opaque_future_namespace_numbers_roundtrip_without_precision_loss_or_downgrade() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    let raw = r#"{"large":1234567890123456789012345678901234567890,"exact":1.2300e+100}"#;
    apply(
        &mut index,
        fence,
        Mutation::Metadata {
            target: target(),
            update: MetadataUpdate {
                namespace: "future".into(),
                expected: None,
                value: Namespace::new(2, raw).unwrap(),
            },
        },
    );
    let loaded = Index::decode(&index.encode().unwrap()).unwrap();
    assert_eq!(
        loaded.entries["synthetic"].metadata["future"].payload.get(),
        raw
    );
    assert!(matches!(
        loaded.apply(
            loaded.cursor(),
            Some(fence),
            Mutation::Metadata {
                target: target(),
                update: MetadataUpdate {
                    namespace: "future".into(),
                    expected: Some(1),
                    value: Namespace::new(1, "{}").unwrap()
                }
            }
        ),
        Err(Error::Incompatible)
    ));
}
#[test]
fn bad_schema_unknown_required_fields_and_oversize_never_reset_to_empty() {
    let (index, _) = initial();
    let value = serde_json::to_value(&index).unwrap();
    let mut future = value.clone();
    future["schema"] = serde_json::json!(2);
    assert!(matches!(
        Index::decode(&serde_json::to_vec(&future).unwrap()),
        Err(Error::Incompatible)
    ));
    let mut unknown = value;
    unknown["required"][0] = serde_json::json!("future-required");
    assert!(matches!(
        Index::decode(&serde_json::to_vec(&unknown).unwrap()),
        Err(Error::Incompatible)
    ));
    assert!(matches!(Index::decode(b"{"), Err(Error::Corrupt)));
    assert!(matches!(
        Index::decode(&vec![b' '; MAX_BYTES + 1]),
        Err(Error::Capacity)
    ));
}
#[test]
fn wrapped_events_require_snapshot_without_evicting_unresolved_requests() {
    let (mut index, mut fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    let old = index.cursor();
    for _ in 0..300 {
        let next = index
            .apply(index.cursor(), None, Mutation::Claim { nonce: [9; 16] })
            .unwrap();
        index = next.next;
        fence = next.fence;
    }
    assert!(matches!(index.tail(old).unwrap(), Tail::SnapshotRequired));
    assert!(matches!(
        index.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Pending(_)
    ));
    assert!(
        matches!(index.tail(index.cursor()).unwrap(),Tail::Events(events) if events.is_empty())
    );
    apply(
        &mut index,
        fence,
        Mutation::Input {
            target: target(),
            operation: InputMutation::Adopt(key(1, 7)),
        },
    );
}
#[test]
fn only_positive_exit_can_retire_and_delayed_exit_cannot_remove_same_name_replacement() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Retire
            }
        ),
        Err(Error::Unavailable)
    ));
    apply(
        &mut index,
        fence,
        Mutation::Exit {
            target: target(),
            operation: ExitMutation::Observe { generation: 42 },
        },
    );
    apply(
        &mut index,
        fence,
        Mutation::Exit {
            target: target(),
            operation: ExitMutation::Retire,
        },
    );
    let mut replacement = identity();
    replacement.host = [10; 16];
    replacement.instance = "replacement".into();
    apply(
        &mut index,
        fence,
        Mutation::ReserveLaunch {
            name: "synthetic".into(),
            entry: Entry::reserved(replacement.clone(), [11; 16], "{}").unwrap(),
        },
    );
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Observe { generation: 99 }
            }
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(index.entries["synthetic"].identity, replacement);
    assert_eq!(index.tombstones.len(), 1);
}
#[test]
fn stale_consumer_checkpoint_does_not_partially_gc_result_or_payload() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    apply(&mut index, fence, complete(1, 7));
    let before = index.encode().unwrap();
    assert!(matches!(
        index.apply(index.cursor(), Some(fence), consume(1, 7, Some(99))),
        Err(Error::Conflict)
    ));
    assert_eq!(index.encode().unwrap(), before);
    assert!(matches!(
        index.lookup(&target(), &key(1, 7)).unwrap(),
        Lookup::Complete(_)
    ));
}
#[test]
fn observed_exit_cannot_discard_unresolved_or_unacknowledged_work() {
    let (mut index, fence) = initial();
    registered(&mut index, fence);
    apply(&mut index, fence, reserve(1, 7));
    apply(
        &mut index,
        fence,
        Mutation::Exit {
            target: target(),
            operation: ExitMutation::Observe { generation: 42 },
        },
    );
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Retire
            }
        ),
        Err(Error::Busy)
    ));
    apply(&mut index, fence, complete(1, 7));
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(fence),
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Retire
            }
        ),
        Err(Error::Busy)
    ));
}
#[test]
fn store_revision_and_writer_epoch_exhaustion_never_wrap_or_mutate() {
    let (index, _) = initial();
    let mut value = serde_json::to_value(&index).unwrap();
    value["revision"] = serde_json::json!(u64::MAX);
    value["events"][0]["revision"] = serde_json::json!(u64::MAX);
    let max = Index::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(
        max.apply(max.cursor(), None, Mutation::Claim { nonce: [9; 16] }),
        Err(Error::Exhausted)
    ));
    value = serde_json::to_value(&index).unwrap();
    value["epoch"] = serde_json::json!(u64::MAX);
    let max = Index::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(
        max.apply(max.cursor(), None, Mutation::Claim { nonce: [9; 16] }),
        Err(Error::Exhausted)
    ));
}
