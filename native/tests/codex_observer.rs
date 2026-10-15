use remuda_native::codex_observer::{Identity, Observer, SelectionEvidence, ThreadState};
use serde_json::{json, Value};
fn identity() -> Identity {
    Identity {
        principal: "registered-fixture".into(),
        instance: "exact-instance".into(),
        server_generation: "owned-server-generation".into(),
    }
}
fn observer(version: &str) -> Observer {
    Observer::new(identity(), version.into()).unwrap()
}
fn reply(id: u64, result: Value) -> Value {
    json!({"id":id,"result":result})
}
fn notify(method: &str, params: Value) -> Value {
    json!({"method":method,"params":params})
}
fn select(o: &mut Observer, g: u64, thread: &str) -> Vec<Value> {
    o.select(SelectionEvidence {
        identity: identity(),
        thread: thread.into(),
        connection_generation: g,
    })
}
fn enumerate(o: &mut Observer) -> (u64, Vec<Value>) {
    let (g, init) = o.connect(0);
    assert_eq!(init["method"], "initialize");
    let out = o.receive(
        g,
        &reply(
            init["id"].as_u64().unwrap(),
            json!({"userAgent":"codex/0.162.1"}),
        ),
        1,
    );
    assert_eq!(out[0]["method"], "initialized");
    assert_eq!(out[1]["method"], "thread/loaded/list");
    (
        g,
        o.receive(
            g,
            &reply(
                out[1]["id"].as_u64().unwrap(),
                json!({"data":["main","child"],"nextCursor":null}),
            ),
            2,
        ),
    )
}
fn ready(o: &mut Observer) -> u64 {
    let (g, out) = enumerate(o);
    assert!(out.is_empty(), "loaded list is not selection proof");
    let out = select(o, g, "main");
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0]["params"],
        json!({"threadId":"main","includeTurns":false})
    );
    o.receive(
        g,
        &reply(
            out[0]["id"].as_u64().unwrap(),
            json!({"thread":{"id":"main","status":{"type":"idle"}}}),
        ),
        3,
    );
    assert_eq!(o.diagnostic(3).thread, ThreadState::Idle);
    g
}
#[test]
fn binding_and_freshness_are_independent_of_transport() {
    let mut o = observer("0.162.1");
    let g = ready(&mut o);
    o.heartbeat(g, 40_000);
    let d = o.diagnostic(40_000);
    assert_eq!(d.transport_age_ms, Some(0));
    assert_eq!(d.thread, ThreadState::Unknown);
    let read = o.reconcile();
    o.receive(
        g,
        &reply(
            read[0]["id"].as_u64().unwrap(),
            json!({"thread":{"id":"main","status":{"type":"idle"}}}),
        ),
        40_001,
    );
    assert_eq!(o.diagnostic(40_001).thread, ThreadState::Idle);
    assert_eq!(
        o.diagnostic(0).thread,
        ThreadState::Unknown,
        "clock reversal"
    );
    o.disconnect();
    let (g2, init) = o.connect(50_000);
    assert_ne!(g, g2);
    o.heartbeat(g2, 50_001);
    assert_eq!(o.diagnostic(50_001).thread, ThreadState::Unknown);
    assert!(o
        .receive(g, &reply(init["id"].as_u64().unwrap(), json!({})), 50_002)
        .is_empty());
    assert!(select(&mut o, g, "main").is_empty());
    assert!(o.diagnostic(50_002).primary_thread.is_none());
    let mut bad = identity();
    bad.instance = "replacement".into();
    assert!(o
        .select(SelectionEvidence {
            identity: bad,
            thread: "main".into(),
            connection_generation: g2
        })
        .is_empty());
    assert!(o.diagnostic(50_002).primary_thread.is_none());
}
#[test]
fn snapshot_event_races_require_another_authoritative_read() {
    let mut o = observer("0.162.1");
    let (g, _) = enumerate(&mut o);
    let read = select(&mut o, g, "main");
    o.receive(
        g,
        &notify(
            "thread/status/changed",
            json!({"threadId":"main","status":{"type":"active","activeFlags":[]}}),
        ),
        4,
    );
    let again = o.receive(
        g,
        &reply(
            read[0]["id"].as_u64().unwrap(),
            json!({"thread":{"id":"main","status":{"type":"idle"}}}),
        ),
        5,
    );
    assert_eq!(o.diagnostic(5).thread, ThreadState::Unknown);
    assert_eq!(again[0]["method"], "thread/read");
    o.receive(g,&reply(again[0]["id"].as_u64().unwrap(),json!({"thread":{"id":"main","status":{"type":"active","activeFlags":["waitingOnApproval","waitingOnUserInput"]}}})),6);
    assert_eq!(
        o.diagnostic(6).thread,
        ThreadState::Active {
            flags: vec!["waitingOnApproval".into(), "waitingOnUserInput".into()]
        }
    );
    let before = serde_json::to_value(o.diagnostic(6)).unwrap();
    o.receive(
        g,
        &notify(
            "future/unknown",
            json!({"threadId":"main","secret":"DO-NOT-STORE"}),
        ),
        7,
    );
    let after = serde_json::to_value(o.diagnostic(7)).unwrap();
    assert_eq!(before["thread"], after["thread"]);
    assert_eq!(before["observed_methods"], after["observed_methods"]);
    assert!(!after.to_string().contains("DO-NOT-STORE"));
    o.receive(
        g,
        &notify(
            "thread/status/changed",
            json!({"threadId":"child","status":{"type":"idle"}}),
        ),
        8,
    );
    assert_eq!(
        serde_json::to_value(o.diagnostic(8)).unwrap()["thread"],
        after["thread"]
    );
    o.receive(
        g,
        &notify(
            "thread/status/changed",
            json!({"threadId":"main","status":{"type":"systemError"}}),
        ),
        9,
    );
    assert_eq!(o.diagnostic(9).thread, ThreadState::Unknown);
}
#[test]
fn overlapping_review_receipts_never_complete_turns_or_authorize_input() {
    let mut o = observer("0.162.1");
    let g = ready(&mut o);
    for id in ["A", "B"] {
        o.receive(
            g,
            &notify("turn/started", json!({"threadId":"main","turn":{"id":id}})),
            4,
        );
    }
    o.receive(
        g,
        &notify(
            "turn/completed",
            json!({"threadId":"main","turn":{"id":"A"}}),
        ),
        5,
    );
    for id in ["R1", "R2"] {
        o.receive(g,&notify("item/autoApprovalReview/started",json!({"threadId":"main","turnId":"B","reviewId":id,"targetItemId":null,"review":{"status":"inProgress","rationale":"DO-NOT-STORE"},"action":{"command":"DO-NOT-STORE"}})),6);
    }
    o.receive(
        g,
        &notify(
            "item/autoApprovalReview/completed",
            json!({"threadId":"main","turnId":"B","reviewId":"R1","review":{"status":"denied"}}),
        ),
        7,
    );
    o.receive(g,&notify("item/autoApprovalReview/started",json!({"threadId":"main","turnId":"B","reviewId":"R1","review":{"status":"inProgress"}})),8);
    let d = o.diagnostic(8);
    assert_eq!(d.turn.unwrap().id, "B");
    assert_eq!(d.review_receipts.len(), 2);
    assert_eq!(d.review_receipts[0].state, "denied");
    assert_eq!(d.review_receipts[1].state, "inProgress");
    let bytes = serde_json::to_string(&o.diagnostic(8)).unwrap();
    assert!(!bytes.contains("DO-NOT-STORE"));
    assert!(!bytes.contains("allow"));
    o.disconnect();
    let d = o.diagnostic(9);
    assert!(d.turn.is_none());
    assert_eq!(d.thread, ThreadState::Unknown);
    assert_eq!(
        d.review_receipts.len(),
        2,
        "receipts survive as explicitly historical data"
    );
}
#[test]
fn version_capabilities_and_absence_are_not_conflated() {
    for version in ["0.162.1", "future-version"] {
        let mut o = observer(version);
        let g = ready(&mut o);
        let d = o.diagnostic(3);
        assert_eq!(d.schema_known_auto_review, version == "0.162.1");
        assert!(!d.experimental_requested);
        assert!(d.observed_methods.is_empty());
        assert!(d.unsupported_methods.is_empty());
        o.receive(g,&notify("item/autoApprovalReview/started",json!({"threadId":"main","turnId":"T","reviewId":"R","review":{"status":"inProgress"}})),4);
        assert_eq!(
            o.diagnostic(4).review_receipts.len(),
            usize::from(version == "0.162.1")
        );
        let read = o.reconcile();
        o.receive(
            g,
            &json!({"id":read[0]["id"],"error":{"code":-32601,"message":"DO-NOT-STORE"}}),
            5,
        );
        assert_eq!(o.diagnostic(5).unsupported_methods, vec!["thread/read"]);
        assert_eq!(o.diagnostic(5).thread, ThreadState::Unknown);
        assert!(o.reconcile().is_empty());
    }
}
#[test]
fn pagination_and_selection_changes_never_guess_a_primary() {
    let mut o = observer("0.162.1");
    let (g, init) = o.connect(0);
    let out = o.receive(g, &reply(init["id"].as_u64().unwrap(), json!({})), 1);
    let next = o.receive(
        g,
        &reply(
            out[1]["id"].as_u64().unwrap(),
            json!({"data":["old"],"nextCursor":"next"}),
        ),
        2,
    );
    assert_eq!(next[0]["params"], json!({"cursor":"next"}));
    assert!(o
        .receive(
            g,
            &reply(
                next[0]["id"].as_u64().unwrap(),
                json!({"data":["main"],"nextCursor":null})
            ),
            3
        )
        .is_empty());
    assert!(o.diagnostic(3).primary_thread.is_none());
    let old = select(&mut o, g, "old");
    let new = select(&mut o, g, "main");
    assert!(o
        .receive(
            g,
            &reply(
                old[0]["id"].as_u64().unwrap(),
                json!({"thread":{"id":"old","status":{"type":"idle"}}})
            ),
            4
        )
        .is_empty());
    assert_eq!(o.diagnostic(4).thread, ThreadState::Unknown);
    o.receive(
        g,
        &reply(
            new[0]["id"].as_u64().unwrap(),
            json!({"thread":{"id":"main","status":{"type":"idle"}}}),
        ),
        5,
    );
    assert_eq!(o.diagnostic(5).thread, ThreadState::Idle);
}

#[cfg(unix)]
fn exchange(
    client: &mut std::os::unix::net::UnixStream,
    server: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    requests: Vec<Value>,
    response: Value,
) {
    use std::io::{BufRead, Write};
    for r in &requests {
        serde_json::to_writer(&mut *client, r).unwrap();
        client.write_all(b"\n").unwrap();
    }
    for expected in requests {
        let mut line = String::new();
        server.read_line(&mut line).unwrap();
        let sent: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent, expected);
        match sent["method"].as_str().unwrap() {
            "initialize" => assert_eq!(
                sent["params"],
                json!({"clientInfo":{"name":"remuda-observer-fixture","version":"0.1"},"capabilities":{"experimentalApi":false}})
            ),
            "initialized" => assert_eq!(sent, json!({"method":"initialized","params":{}})),
            "thread/loaded/list" => {
                assert!(sent["params"] == json!({}) || sent["params"] == json!({"cursor":"next"}))
            }
            "thread/read" => assert_eq!(
                sent["params"],
                json!({"threadId":"main","includeTurns":false})
            ),
            method => panic!("forbidden outbound {method}"),
        }
    }
    serde_json::to_writer(server.get_mut(), &response).unwrap();
    server.get_mut().write_all(b"\n").unwrap();
}

#[cfg(unix)]
#[test]
fn fake_uds_exercises_complete_outbound_allowlist_and_reconnect() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let mut server = BufReader::new(server);
    let mut input = BufReader::new(client.try_clone().unwrap());
    let mut o = observer("0.162.1");
    for round in 0..4 {
        let (g, init) = o.connect(round * 100);
        let id = init["id"].as_u64().unwrap();
        let init_response = if round == 2 {
            json!({"id":id,"error":{"code":-32601,"message":"DO-NOT-STORE"}})
        } else {
            reply(id, json!({}))
        };
        exchange(&mut client, &mut server, vec![init], init_response);
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        let out = o.receive(
            g,
            &serde_json::from_str::<Value>(&line).unwrap(),
            1 + round * 100,
        );
        if round == 2 {
            assert!(out.is_empty());
            assert_eq!(o.diagnostic(1 + round * 100).thread, ThreadState::Unknown);
            o.disconnect();
            continue;
        }
        let id = out[1]["id"].as_u64().unwrap();
        let response = match round {
            0 => reply(id, json!({"data":["child"],"nextCursor":"next"})),
            3 => json!({"id":id,"error":{"code":-32000,"message":"DO-NOT-STORE"}}),
            _ => reply(id, json!({"data":["main"],"nextCursor":null})),
        };
        exchange(&mut client, &mut server, out, response);
        line.clear();
        input.read_line(&mut line).unwrap();
        let out = o.receive(
            g,
            &serde_json::from_str::<Value>(&line).unwrap(),
            2 + round * 100,
        );
        if round == 0 {
            assert_eq!(out[0]["method"], "thread/loaded/list");
            let id = out[0]["id"].as_u64().unwrap();
            exchange(
                &mut client,
                &mut server,
                out,
                reply(id, json!({"data":["main"],"nextCursor":null})),
            );
            line.clear();
            input.read_line(&mut line).unwrap();
            assert!(o
                .receive(g, &serde_json::from_str::<Value>(&line).unwrap(), 3)
                .is_empty());
        } else {
            assert!(out.is_empty());
        }
        if round == 3 {
            assert!(select(&mut o, g, "main").is_empty());
            assert!(!serde_json::to_string(&o.diagnostic(2 + round * 100))
                .unwrap()
                .contains("DO-NOT-STORE"));
            o.disconnect();
            continue;
        }
        let out = select(&mut o, g, "main");
        let id = out[0]["id"].as_u64().unwrap();
        let response = if round == 0 {
            reply(id, json!({"thread":{"id":"main","status":{"type":"idle"}}}))
        } else {
            json!({"id":id,"error":{"code":-32601,"message":"DO-NOT-STORE"}})
        };
        exchange(&mut client, &mut server, out, response);
        line.clear();
        input.read_line(&mut line).unwrap();
        assert!(o
            .receive(
                g,
                &serde_json::from_str::<Value>(&line).unwrap(),
                4 + round * 100
            )
            .is_empty());
        assert!(!serde_json::to_string(&o.diagnostic(4 + round * 100))
            .unwrap()
            .contains("DO-NOT-STORE"));
        o.disconnect();
        assert_eq!(o.diagnostic(5 + round * 100).thread, ThreadState::Unknown);
    }
}

#[test]
fn lifecycle_transitions_invalidate_older_status_and_late_completion_is_ignored() {
    let mut o = observer("0.162.1");
    let g = ready(&mut o);
    let read = o.receive(
        g,
        &notify("turn/started", json!({"threadId":"main","turn":{"id":"B"}})),
        4,
    );
    assert_eq!(
        o.diagnostic(4).thread,
        ThreadState::Unknown,
        "older idle must not remain fresh"
    );
    assert_eq!(o.diagnostic(4).turn.unwrap().id, "B");
    assert_eq!(read.len(), 1);
    o.receive(
        g,
        &reply(
            read[0]["id"].as_u64().unwrap(),
            json!({"thread":{"id":"main","status":{"type":"active","activeFlags":[]}}}),
        ),
        5,
    );
    assert!(matches!(o.diagnostic(5).thread, ThreadState::Active { .. }));
    let out = o.receive(
        g,
        &notify(
            "turn/completed",
            json!({"threadId":"main","turn":{"id":"A"}}),
        ),
        6,
    );
    assert!(out.is_empty());
    assert_eq!(o.diagnostic(6).turn.unwrap().id, "B");
    assert!(matches!(o.diagnostic(6).thread, ThreadState::Active { .. }));
    let read = o.receive(
        g,
        &notify(
            "turn/completed",
            json!({"threadId":"main","turn":{"id":"B"}}),
        ),
        7,
    );
    assert_eq!(o.diagnostic(7).thread, ThreadState::Unknown);
    assert_eq!(o.diagnostic(7).turn.unwrap().state, "completed");
    assert_eq!(read.len(), 1);
    o.receive(
        g,
        &reply(
            read[0]["id"].as_u64().unwrap(),
            json!({"thread":{"id":"main","status":{"type":"idle"}}}),
        ),
        8,
    );
    assert_eq!(o.diagnostic(8).thread, ThreadState::Idle);
    assert!(o.diagnostic(8).turn.is_none());
}
