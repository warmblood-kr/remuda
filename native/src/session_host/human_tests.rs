use super::*;

const A: ConnectionId = ConnectionId(1);
const B: ConnectionId = ConnectionId(2);

fn request(id: u8, expected: u64) -> GrantRequest {
    GrantRequest {
        request: [id; 16],
        expected,
        recovery: [id; 32],
    }
}

#[test]
fn grant_cas_is_exact_and_lost_ack_retry_is_evidence_not_transfer() {
    let mut human = Human::new([1; 16]);
    assert_eq!(human.grant(A, request(1, 1)), Err(GrantError::Stale));
    assert_eq!(human.grant(A, request(1, 0)), Ok((1, GrantState::Current)));
    // Same request from another connection is evidence; holder stays A.
    assert_eq!(human.grant(B, request(1, 0)), Ok((1, GrantState::Current)));
    assert!(human.holds(A, 1) && !human.holds(B, 1));
    let changed = GrantRequest {
        recovery: [9; 32],
        ..request(1, 0)
    };
    assert_eq!(human.grant(A, changed), Err(GrantError::Conflict));
    // Old expectation never becomes a fresh takeover.
    assert_eq!(human.grant(B, request(2, 0)), Err(GrantError::Stale));
    assert_eq!(human.grant(B, request(2, 1)), Ok((2, GrantState::Current)));
    assert_eq!(human.high_water(), 2);
    human.disconnect(A);
    assert_eq!(human.current(), Some(2), "stale cleanup kept successor");
    human.disconnect(B);
    assert_eq!(human.grant(B, request(2, 1)), Ok((2, GrantState::Released)));
    assert_eq!(human.current(), None);
}

#[test]
fn superseded_grant_and_exhaustion_fail_before_any_change() {
    let mut human = Human::new([1; 16]);
    human.grant(A, request(1, 0)).unwrap();
    human.grant(B, request(2, 1)).unwrap();
    assert!(!human.revoke(1), "stale revoke ignored");
    assert_eq!(human.current(), Some(2));
    human.high_water = u64::MAX;
    assert_eq!(
        human.grant(A, request(3, u64::MAX)),
        Err(GrantError::Exhausted)
    );
    assert_eq!(human.current(), Some(2));
}

#[test]
fn raw_sequence_is_global_ordered_and_retries_answer_before_grant_check() {
    let mut human = Human::new([1; 16]);
    human.grant(A, request(1, 0)).unwrap();
    assert_eq!(human.reserve(1, 2, [5; 32]), Raw::OutOfOrder);
    assert_eq!(human.reserve(1, 1, [5; 32]), Raw::New);
    assert_eq!(human.reserve(1, 1, [5; 32]), Raw::Pending);
    assert_eq!(human.reserve(1, 1, [6; 32]), Raw::Conflict);
    assert!(human.complete(1, Delivery::Written));
    assert!(!human.complete(1, Delivery::Rejected), "completes once");
    human.grant(B, request(2, 1)).unwrap();
    // Old grant: exact retry still returns evidence; new raw is Stale.
    assert_eq!(
        human.reserve(1, 1, [5; 32]),
        Raw::Complete(Delivery::Written)
    );
    assert_eq!(human.reserve(1, 2, [5; 32]), Raw::Stale);
    assert_eq!(human.reserve(2, 1, [5; 32]), Raw::Conflict);
    assert_eq!(human.reserve(2, 2, [5; 32]), Raw::New);
}

#[test]
fn capacity_refuses_without_eviction_or_spending_a_sequence() {
    let mut human = Human::new([1; 16]);
    human.grant(A, request(1, 0)).unwrap();
    for sequence in 1..=MAX_RAW as u64 {
        assert_eq!(human.reserve(1, sequence, [0; 32]), Raw::New);
        assert!(human.complete(sequence, Delivery::Written));
    }
    let next = MAX_RAW as u64 + 1;
    assert_eq!(human.reserve(1, next, [0; 32]), Raw::Busy);
    assert_eq!(
        human.recover(1, 1, [0; 32]),
        Raw::Complete(Delivery::Written)
    );
    let cut = human.cut(10).unwrap();
    assert_eq!(human.ack(A, cut), Retire::Retired);
    assert_eq!(human.reserve(1, next, [0; 32]), Raw::New);
    assert_eq!(human.recover(1, 3, [0; 32]), Raw::Forgotten);
    assert_eq!(human.reserve(1, 3, [0; 32]), Raw::Forgotten, "never New");
}

#[test]
fn ack_binds_exact_cut_owner_and_completed_prefix() {
    let mut human = Human::new([1; 16]);
    human.grant(A, request(1, 0)).unwrap();
    human.reserve(1, 1, [1; 32]);
    human.complete(1, Delivery::Written);
    human.reserve(1, 2, [2; 32]);
    assert_eq!(human.cut(2), Err(Retire::Pending));
    let one = human.cut(1).unwrap();
    let forged = Cut {
        hash: [0; 32],
        ..one
    };
    assert_eq!(human.ack(A, forged), Retire::Conflict);
    assert_eq!(human.ack(B, one), Retire::Stale, "not the viewer");
    assert_eq!(human.ack(A, one), Retire::Retired);
    assert_eq!(human.ack(A, one), Retire::Retired, "exact latest retry");
    assert_eq!(
        human.discard(one),
        Retire::Live,
        "live grant refuses discard"
    );
    human.complete(2, Delivery::Indeterminate);
    let two = human.cut(2).unwrap();
    assert_eq!(two.first, 2);
    assert_ne!(two.hash, one.hash);
    assert_eq!(human.ack(A, two), Retire::Retired);
    assert_eq!(
        human.ack(A, one),
        Retire::Forgotten,
        "older receipt evicted"
    );
    assert!(!human.settled(), "live grant blocks Stop");
    human.disconnect(A);
    assert!(human.settled());
    assert_eq!(
        human.discard(two),
        Retire::Forgotten,
        "ACK receipt never answers discard"
    );
}

#[test]
fn discard_needs_inactive_grant_and_fifo_blocks_successor_prefix() {
    let mut human = Human::new([1; 16]);
    human.grant(A, request(1, 0)).unwrap();
    human.reserve(1, 1, [1; 32]);
    human.complete(1, Delivery::Written);
    human.grant(B, request(2, 1)).unwrap();
    human.reserve(2, 2, [2; 32]);
    human.complete(2, Delivery::Written);
    // Successor cannot ACK past an abandoned older cut.
    assert_eq!(human.cut(2), Err(Retire::Conflict));
    let old = human.cut(1).unwrap();
    assert_eq!(human.ack(B, old), Retire::Stale);
    let mine = Cut {
        grant: 2,
        first: 2,
        last: 2,
        hash: [0; 32],
    };
    assert_eq!(human.ack(B, mine), Retire::Conflict);
    assert_eq!(human.discard(old), Retire::Retired);
    let mine = human.cut(2).unwrap();
    assert_eq!(human.discard(mine), Retire::Live);
    assert_eq!(human.ack(B, mine), Retire::Retired);
}
