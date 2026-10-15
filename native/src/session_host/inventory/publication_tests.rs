use super::*;
use crate::session_host::inventory::model_tests::{apply, identity, initial, target};

fn reserved() -> (Index, Fence, Lease) {
    let (mut index, fence) = initial();
    let mut entry = Entry::reserved(identity(), [4; 16], "{}").unwrap();
    entry.birth = Some(Birth::new("/private/synthetic-host".into(), [7; 16]).unwrap());
    apply(
        &mut index,
        fence,
        Mutation::ReserveLaunch {
            name: "synthetic".into(),
            entry,
        },
    );
    let lease = Lease::new(index.cursor().store, target(), [4; 16], [7; 16]);
    (index, fence, lease)
}
#[test]
fn scoped_host_lease_survives_controller_epoch_change_without_global_writer_authority() {
    let (mut index, old, lease) = reserved();
    let claimed = index
        .apply(index.cursor(), None, Mutation::Claim { nonce: [9; 16] })
        .unwrap();
    index = claimed.next;
    assert!(matches!(
        index.apply(index.cursor(), Some(old), Mutation::Register(target())),
        Err(Error::StaleFence)
    ));
    assert!(matches!(
        index.apply(
            index.cursor(),
            Some(claimed.fence),
            Mutation::Register(target())
        ),
        Err(Error::Invalid(_))
    ));
    let ready = index
        .publish(index.cursor(), &lease, Publication::Ready)
        .unwrap();
    assert_eq!(ready.entries["synthetic"].phase, Phase::Registered);
    let repeated = ready
        .publish(ready.cursor(), &lease, Publication::Ready)
        .unwrap();
    assert_eq!(ready.cursor(), repeated.cursor());
    assert_eq!(ready.events.len(), repeated.events.len());
    assert!(matches!(
        ready.apply(
            ready.cursor(),
            Some(old),
            Mutation::Exit {
                target: target(),
                operation: ExitMutation::Observe { generation: 1 }
            }
        ),
        Err(Error::StaleFence)
    ));
}
#[test]
fn wrong_store_identity_launch_nonce_and_revision_refuse_without_mutation() {
    let (index, _, lease) = reserved();
    let before = index.encode().unwrap();
    let mut wrong = lease.clone();
    wrong.nonce = [8; 16];
    assert!(matches!(
        index.publish(index.cursor(), &wrong, Publication::Ready),
        Err(Error::StaleFence)
    ));
    let mut wrong = lease.clone();
    wrong.launch = [8; 16];
    assert!(matches!(
        index.publish(index.cursor(), &wrong, Publication::Ready),
        Err(Error::Conflict)
    ));
    let mut wrong = lease.clone();
    wrong.identity.instance = "replacement".into();
    assert!(matches!(
        index.publish(index.cursor(), &wrong, Publication::Ready),
        Err(Error::Conflict)
    ));
    let mut wrong = lease.clone();
    wrong.store = [8; 16];
    assert!(matches!(
        index.publish(index.cursor(), &wrong, Publication::Ready),
        Err(Error::StaleStore)
    ));
    let mut stale = index.cursor();
    stale.number -= 1;
    assert!(matches!(
        index.publish(stale, &lease, Publication::Ready),
        Err(Error::StaleRevision)
    ));
    assert_eq!(index.encode().unwrap(), before);
}
#[test]
fn exit_and_terminal_end_are_independent_positive_immutable_facts_not_timeout_retirement() {
    let (mut index, fence, lease) = reserved();
    assert!(matches!(
        index.publish(
            index.cursor(),
            &lease,
            Publication::Exited {
                generation: 1,
                code: Some(0),
                signal: None
            }
        ),
        Err(Error::Unavailable)
    ));
    index = index
        .publish(index.cursor(), &lease, Publication::Ready)
        .unwrap();
    assert!(matches!(
        index.publish(
            index.cursor(),
            &lease,
            Publication::Exited {
                generation: 1,
                code: None,
                signal: None
            }
        ),
        Err(Error::Invalid(_))
    ));
    index = index
        .publish(
            index.cursor(),
            &lease,
            Publication::TerminalEnd {
                generation: 1,
                kind: TerminalEnd::Eof,
            },
        )
        .unwrap();
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
    index = index
        .publish(
            index.cursor(),
            &lease,
            Publication::Exited {
                generation: 1,
                code: Some(259),
                signal: None,
            },
        )
        .unwrap();
    assert!(matches!(
        index.publish(
            index.cursor(),
            &lease,
            Publication::Exited {
                generation: 1,
                code: Some(0),
                signal: None
            }
        ),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        index.publish(index.cursor(), &lease, Publication::Ready),
        Err(Error::Conflict)
    ));
    apply(
        &mut index,
        fence,
        Mutation::Exit {
            target: target(),
            operation: ExitMutation::Retire,
        },
    );
    assert!(index.entries.is_empty());
    assert!(matches!(
        index.publish(index.cursor(), &lease, Publication::Ready),
        Err(Error::Unavailable)
    ));
}
#[test]
fn near_full_store_preserves_ready_exit_end_progress_and_versioned_lease_on_decode() {
    let (index, fence, lease) = reserved();
    let payload = format!("\"{}\"", "x".repeat(MAX_METADATA - 2));
    let index = crate::session_host::inventory::model_tests::fill_metadata(index, fence, &payload);
    let mut index = Index::decode(&index.encode().unwrap()).unwrap();
    index = index
        .publish(index.cursor(), &lease, Publication::Ready)
        .unwrap();
    index = index
        .publish(
            index.cursor(),
            &lease,
            Publication::Exited {
                generation: u64::MAX,
                code: None,
                signal: Some(15),
            },
        )
        .unwrap();
    index = index
        .publish(
            index.cursor(),
            &lease,
            Publication::TerminalEnd {
                generation: u64::MAX,
                kind: TerminalEnd::Hangup,
            },
        )
        .unwrap();
    let index = Index::decode(&index.encode().unwrap()).unwrap();
    assert_eq!(
        index.entries["synthetic"].phase,
        Phase::Exited {
            generation: u64::MAX
        }
    );
    assert_eq!(
        index.entries["synthetic"]
            .birth
            .as_ref()
            .unwrap()
            .endpoint(),
        "/private/synthetic-host"
    );
    let mut unsupported: serde_json::Value =
        serde_json::from_slice(&index.encode().unwrap()).unwrap();
    unsupported["required"]
        .as_array_mut()
        .unwrap()
        .push("future-publisher-v2".into());
    assert!(matches!(
        Index::decode(&serde_json::to_vec(&unsupported).unwrap()),
        Err(Error::Incompatible)
    ));
}
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn disk_reserved(
    stage: u8,
) -> (
    Scratch,
    crate::session_host::inventory::disk::Store,
    Fence,
    Lease,
) {
    use crate::session_host::inventory::disk::Store;
    let root = Scratch(
        std::env::temp_dir().join(format!("rm-publication-{}-{stage}", std::process::id())),
    );
    let store = Store::create(&root.0).unwrap();
    let claim = store
        .commit(
            store.read().unwrap().cursor(),
            None,
            Mutation::Claim { nonce: [1; 16] },
            std::time::Duration::ZERO,
        )
        .unwrap();
    let mut entry = Entry::reserved(identity(), [4; 16], "{}").unwrap();
    entry.birth = Some(Birth::new(root.0.join("host").to_string_lossy().into(), [7; 16]).unwrap());
    store
        .commit(
            claim.next.cursor(),
            Some(claim.fence),
            Mutation::ReserveLaunch {
                name: "synthetic".into(),
                entry,
            },
            std::time::Duration::ZERO,
        )
        .unwrap();
    let lease = Lease::new(
        store.read().unwrap().cursor().store,
        target(),
        [4; 16],
        [7; 16],
    );
    (root, store, claim.fence, lease)
}
#[test]
fn scoped_disk_publication_survives_writer_change_and_uncertain_replace_without_reset() {
    use crate::session_host::inventory::disk::{self, Error as DiskError, Store};
    for stage in [1, 2] {
        let (root, store, old, lease) = disk_reserved(stage);
        store
            .commit(
                store.read().unwrap().cursor(),
                None,
                Mutation::Claim { nonce: [9; 16] },
                std::time::Duration::ZERO,
            )
            .unwrap();
        assert!(matches!(
            store.commit(
                store.read().unwrap().cursor(),
                Some(old),
                Mutation::Register(target()),
                std::time::Duration::ZERO
            ),
            Err(DiskError::Data(Error::StaleFence))
        ));
        disk::fail_at(stage);
        assert!(matches!(
            store.publish(
                store.read().unwrap().cursor(),
                &lease,
                Publication::Ready,
                std::time::Duration::ZERO
            ),
            Err(DiskError::CommitUnknown(_))
        ));
        let reopened = Store::open(&root.0).unwrap();
        let phase = reopened.read().unwrap().entries["synthetic"].phase;
        assert_eq!(
            phase,
            if stage == 1 {
                Phase::Reserved
            } else {
                Phase::Registered
            }
        );
        let revision: Revision = reopened
            .publish(
                reopened.read().unwrap().cursor(),
                &lease,
                Publication::Ready,
                std::time::Duration::ZERO,
            )
            .unwrap();
        assert_eq!(reopened.read().unwrap().cursor(), revision);
        assert_eq!(
            reopened.read().unwrap().entries["synthetic"].phase,
            Phase::Registered
        );
    }
}
