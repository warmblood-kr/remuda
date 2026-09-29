use super::*;

fn entry(fp: &str, state: NodeState, version: u64, by: &str) -> AuthorizedNode {
    let mut key = [0u8; 32];
    for (index, byte) in fp.bytes().enumerate() {
        key[index % 32] ^= byte;
    }
    AuthorizedNode {
        node_fp: encoding::fingerprint(&key),
        static_pubkey: encoding::encode_base64(&key),
        delivered_by: None,
        format_major: 1,
        format_minor: 0,
        optional_fields: std::collections::BTreeMap::new(),
        endpoint: None,
        state,
        version,
        by: by.to_owned(),
    }
}
fn public_key(e: &AuthorizedNode) -> Vec<u8> {
    encoding::decode_base64(&e.static_pubkey).unwrap()
}
fn upd(entries: &[AuthorizedNode], sender_fp: &str) -> RegistryUpdate {
    RegistryUpdate {
        sender_fp: sender_fp.to_owned(),
        entries: entries.to_vec(),
    }
}
fn find<'a>(r: &'a Registry, fp: &str) -> &'a AuthorizedNode {
    r.authorized_nodes.iter().find(|e| e.node_fp == fp).unwrap()
}
fn ordered_pair() -> (AuthorizedNode, AuthorizedNode) {
    let mut x = entry("probe-owner", NodeState::Admitted, 1, "");
    let mut y = entry("probe-relay", NodeState::Admitted, 1, "");
    x.by = x.node_fp.clone();
    y.by = x.node_fp.clone();
    if x.node_fp < y.node_fp {
        (x, y)
    } else {
        (y, x)
    }
}

// M-1 original probe (relay keeps origin `by`, as the decided protocol does).
#[test]
fn p95_owner_endpoint_update_is_not_shadowed_by_earlier_relay() {
    let (owner, relay_node) = ordered_pair();
    let mut c = entry("probe-receiver", NodeState::Admitted, 1, "");
    c.by = owner.by.clone();
    let mut owner_old = owner.clone();
    owner_old.endpoint = Some("192.0.2.10:9443".into());
    let mut c_view = Registry {
        authorized_nodes: vec![owner_old.clone(), relay_node.clone(), c.clone()],
    };
    let mut owner_new = owner_old.clone();
    owner_new.endpoint = Some("192.0.2.20:9443".into());
    owner_new.version += 1;
    apply_update(
        &mut c_view,
        &upd(&[owner_new.clone()], &relay_node.node_fp),
        &public_key(&relay_node),
        &c.node_fp,
    )
    .unwrap();
    apply_update(
        &mut c_view,
        &upd(&[owner_new.clone()], &owner.node_fp),
        &public_key(&owner),
        &c.node_fp,
    )
    .unwrap();
    assert_eq!(
        find(&c_view, &owner.node_fp).endpoint.as_deref(),
        Some("192.0.2.20:9443")
    );
}

// M-2 original probe (no by-rewrite).
#[test]
fn p95_two_converged_nodes_have_equal_digests() {
    let (a, b) = ordered_pair();
    let mut x = entry("probe-x", NodeState::Admitted, 1, "");
    x.by = a.node_fp.clone();
    let mut a_view = Registry {
        authorized_nodes: vec![a.clone(), b.clone(), x],
    };
    let mut b_view = Registry {
        authorized_nodes: vec![a.clone(), b.clone()],
    };
    for _ in 0..8 {
        apply_update(
            &mut b_view,
            &upd(&a_view.authorized_nodes, &a.node_fp),
            &public_key(&a),
            &b.node_fp,
        )
        .unwrap();
        apply_update(
            &mut a_view,
            &upd(&b_view.authorized_nodes, &b.node_fp),
            &public_key(&b),
            &a.node_fp,
        )
        .unwrap();
    }
    assert!(apply_update(
        &mut b_view,
        &upd(&a_view.authorized_nodes, &a.node_fp),
        &public_key(&a),
        &b.node_fp
    )
    .unwrap()
    .applied
    .is_empty());
    assert!(apply_update(
        &mut a_view,
        &upd(&b_view.authorized_nodes, &b.node_fp),
        &public_key(&b),
        &a.node_fp
    )
    .unwrap()
    .applied
    .is_empty());
    assert_eq!(a_view.digest().unwrap(), b_view.digest().unwrap());
    // delivered_by is local: set on receipt, never in digest
    assert!(b_view
        .authorized_nodes
        .iter()
        .any(|e| e.delivered_by.is_some()));
}

// NEW: revoking an admitter must not poison every later push that carries
// entries it admitted before its revocation (already identical on receiver).
#[test]
fn p95_revoked_admitter_does_not_poison_full_push() {
    let (a, b) = ordered_pair(); // a = founder/admitter of b, x
    let mut c = entry("probe-c", NodeState::Admitted, 1, "");
    c.by = a.node_fp.clone();
    let mut x = entry("probe-x2", NodeState::Admitted, 1, "");
    x.by = a.node_fp.clone();
    let mut a_revoked = a.clone();
    a_revoked.state = NodeState::Revoked;
    a_revoked.version += 1;
    a_revoked.by = b.node_fp.clone();
    let full = vec![a_revoked.clone(), b.clone(), c.clone(), x.clone()];
    // c already has everything, including a's tombstone: b's routine full push.
    let mut c_view = Registry {
        authorized_nodes: full.clone(),
    };
    let r = apply_update(
        &mut c_view,
        &upd(&full, &b.node_fp),
        &public_key(&b),
        &c.node_fp,
    );
    assert!(
        r.is_ok(),
        "identical full push refused after admitter revoked: {r:?}"
    );
}

// M-4: the non-member mutant must be killed by an update that would
// otherwise be accepted (origin = an admitted member of the receiver).
#[test]
fn p95_nonmember_sender_with_valid_origin_refused() {
    let (a, _) = ordered_pair();
    let outsider = entry("probe-outsider", NodeState::Admitted, 1, "");
    let mut x = entry("probe-x3", NodeState::Admitted, 1, "");
    x.by = a.node_fp.clone();
    let mut view = Registry {
        authorized_nodes: vec![a.clone()],
    };
    assert!(apply_update(
        &mut view,
        &upd(&[x], &outsider.node_fp),
        &public_key(&outsider),
        &a.node_fp
    )
    .is_err());
}

#[test]
fn p3_optional_field_union_stays_within_cap() {
    let mut first = entry("p3-optional-cap", NodeState::Admitted, 4, "origin");
    first.by = first.node_fp.clone();
    let mut second = first.clone();
    first.optional_fields = (0..8)
        .map(|index| (format!("left_{index}"), serde_json::json!(index)))
        .collect();
    second.optional_fields = (0..8)
        .map(|index| (format!("right_{index}"), serde_json::json!(index)))
        .collect();

    let mut forward = Registry::default();
    forward
        .merge(&Registry {
            authorized_nodes: vec![first.clone(), second.clone()],
        })
        .unwrap();
    let mut reverse = Registry::default();
    reverse
        .merge(&Registry {
            authorized_nodes: vec![second, first],
        })
        .unwrap();

    let merged = find(
        &forward,
        &entry("p3-optional-cap", NodeState::Admitted, 4, "origin").node_fp,
    );
    assert_eq!(merged.optional_fields.len(), 8);
    assert_eq!(forward, reverse);
    RegistryUpdate {
        sender_fp: merged.node_fp.clone(),
        entries: vec![merged.clone()],
    }
    .encode()
    .expect("winner stays within the per-entry optional-field cap");
}

#[test]
fn join_bootstrap_imports_issuer_view_once_then_uses_origin_checks() {
    let mut issuer = entry("bootstrap-issuer", NodeState::Admitted, 1, "issuer");
    issuer.by = issuer.node_fp.clone();
    let mut joiner = entry("bootstrap-joiner", NodeState::Admitted, 1, "issuer");
    joiner.by = issuer.node_fp.clone();
    let mut inherited = entry("bootstrap-inherited", NodeState::Admitted, 1, "unknown");
    inherited.by = entry(
        "bootstrap-unknown-origin",
        NodeState::Admitted,
        1,
        "unknown",
    )
    .node_fp;
    let mut view = Registry {
        authorized_nodes: vec![issuer.clone(), joiner.clone()],
    };
    let snapshot = upd(
        &[issuer.clone(), joiner.clone(), inherited.clone()],
        &issuer.node_fp,
    );

    apply_join_bootstrap_snapshot(&mut view, &snapshot, &public_key(&issuer), &joiner.node_fp)
        .unwrap();
    assert!(view
        .authorized_nodes
        .iter()
        .any(|known| known.node_fp == inherited.node_fp));

    let mut ordinary_new = entry("bootstrap-steady-state", NodeState::Admitted, 1, "unknown");
    ordinary_new.by = inherited.by.clone();
    let outcome = apply_update(
        &mut view,
        &upd(&[ordinary_new.clone()], &issuer.node_fp),
        &public_key(&issuer),
        &issuer.node_fp,
    )
    .unwrap();
    assert_eq!(outcome.dropped_origin_entries, 1);
    assert!(!view
        .authorized_nodes
        .iter()
        .any(|known| known.node_fp == ordinary_new.node_fp));
}

#[test]
fn join_bootstrap_drops_issuer_self_tombstone() {
    let mut issuer = entry("bootstrap-self-issuer", NodeState::Admitted, 1, "issuer");
    issuer.by = issuer.node_fp.clone();
    let mut joiner = entry("bootstrap-self-joiner", NodeState::Admitted, 1, "issuer");
    joiner.by = issuer.node_fp.clone();
    let mut self_tombstone = joiner.clone();
    self_tombstone.state = NodeState::Revoked;
    self_tombstone.version += 1;
    self_tombstone.by = issuer.node_fp.clone();
    let mut view = Registry {
        authorized_nodes: vec![issuer.clone(), joiner.clone()],
    };

    apply_join_bootstrap_snapshot(
        &mut view,
        &upd(&[issuer.clone(), self_tombstone], &issuer.node_fp),
        &public_key(&issuer),
        &joiner.node_fp,
    )
    .unwrap();

    assert_eq!(find(&view, &joiner.node_fp).state, NodeState::Admitted);
}
