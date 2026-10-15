use super::*;
use crate::{ManualClock, ScriptedAgent};

fn session() -> Arc<Session> {
    Arc::new(Session::new(
        "owned",
        Box::new(ScriptedAgent::new(vec!["screen".into()])),
        Arc::new(ManualClock::new()),
    ))
}

#[test]
fn owned_guard_keeps_the_exact_session_and_releases_without_terminating_it() {
    let session = session();
    let weak = Arc::downgrade(&session);
    let identity = session.instance_id().to_owned();
    let held = session.attach_owned().unwrap();
    drop(session);
    assert_eq!(held.session().instance_id(), identity);
    assert!(held.session().is_attached());
    held.write_raw(b"draft").unwrap();
    assert!(held.session().human_idle_for().is_some());
    let retained = weak.upgrade().unwrap();
    drop(held);
    assert!(!retained.is_attached());
    assert!(retained.is_alive());
    retained.send_line("after detach").unwrap();
    drop(retained);
    assert!(weak.upgrade().is_none());
}

#[test]
fn borrowed_and_owned_guards_share_takeover_and_checked_drop() {
    let session = session();
    let borrowed = session.attach().unwrap();
    let owned = session.attach_owned().unwrap();
    assert!(borrowed.is_displaced());
    assert!(owned.generation() > borrowed.generation());
    assert!(matches!(
        borrowed.write_raw(b"stale"),
        Err(AgentError::Attached)
    ));
    drop(borrowed);
    assert!(session.is_attached());
    owned.write_raw(b"current").unwrap();
    let successor = session.attach().unwrap();
    assert!(owned.is_displaced());
    drop(owned);
    assert!(session.is_attached());
    successor.write_raw(b"successor").unwrap();
    drop(successor);
    assert!(!session.is_attached());
}

#[test]
fn generation_exhaustion_refuses_both_grant_forms_without_displacing_holder() {
    let session = session();
    session
        .last_attach_generation
        .store(u64::MAX - 2, Ordering::SeqCst);
    let penultimate = session.attach().unwrap();
    assert_eq!(penultimate.generation(), u64::MAX - 1);
    let final_holder = session.attach_owned().unwrap();
    assert_eq!(final_holder.generation(), u64::MAX);
    final_holder.write_raw(b"human draft").unwrap();
    let human_at = *session.last_human_input_at.lock().unwrap();
    let input_at = *session.last_input_at.lock().unwrap();
    assert!(penultimate.is_displaced());
    assert!(session.was_attachment_taken_over(penultimate.generation()));
    assert!(!session.was_attachment_taken_over(final_holder.generation()));
    assert!(session.attach().is_err());
    assert!(session.attach_owned().is_err());
    assert_final_holder_unchanged(&session, &final_holder, human_at, input_at);
    drop(penultimate);
    assert!(session.is_attached());
    final_holder
        .write_raw(b"final grant remains usable")
        .unwrap();
    drop(final_holder);
    assert!(!session.is_attached());
    assert!(session.attach_owned().is_err());
    assert!(!session.is_attached());
}

fn assert_final_holder_unchanged(
    session: &Session,
    final_holder: &OwnedAttached,
    human_at: Option<Duration>,
    input_at: Duration,
) {
    assert!(!final_holder.is_displaced());
    assert!(session.is_attached());
    assert_eq!(*session.last_human_input_at.lock().unwrap(), human_at);
    assert_eq!(*session.last_input_at.lock().unwrap(), input_at);
    assert_eq!(session.human_inflight.load(Ordering::SeqCst), 0);
    let slot = session.attach_slot.lock().unwrap();
    assert_eq!(slot.as_ref().unwrap().0, u64::MAX);
    assert!(Arc::ptr_eq(
        &slot.as_ref().unwrap().1,
        &final_holder.displaced
    ));
    drop(slot);
    assert_eq!(
        session.last_attach_generation.load(Ordering::SeqCst),
        u64::MAX
    );
}

#[test]
fn concurrent_borrowed_and_owned_grants_share_one_holder_and_checked_release() {
    use crate::session_operations::AttachmentOperations;
    let session = session();
    let barrier = std::sync::Barrier::new(9);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let session = &session;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let guard: Box<dyn AttachmentOperations + '_> = if index % 2 == 0 {
                        Box::new(session.attach_owned().unwrap())
                    } else {
                        Box::new(session.attach().unwrap())
                    };
                    guard
                })
            })
            .collect();
        barrier.wait();
        let mut guards: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        guards.sort_by_key(|guard| guard.generation());
        assert_eq!(
            guards
                .iter()
                .filter(|guard| !guard.displaced().unwrap())
                .count(),
            1
        );
        assert_eq!(
            guards
                .iter()
                .map(|guard| guard.generation())
                .collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        let current = guards.pop().unwrap();
        drop(guards);
        assert!(session.is_attached());
        current
            .write_raw(b"one holder", &|| false, Arc::new(|| {}))
            .unwrap();
        drop(current);
        assert!(!session.is_attached());
    });
}

#[test]
fn owned_guard_uses_the_existing_checked_operations_trait() {
    use crate::session_operations::AttachmentOperations;
    let session = session();
    let held: Box<dyn AttachmentOperations> = Box::new(session.attach_owned().unwrap());
    assert_eq!(held.identity().instance, session.instance_id());
    assert_eq!(held.generation(), 1);
    held.write_raw(b"x", &|| false, Arc::new(|| {})).unwrap();
    assert!(!held.displaced().unwrap());
    drop(held);
    assert!(!session.is_attached());
}

#[test]
fn concurrent_exhaustion_grants_the_final_generation_to_only_one_holder() {
    let session = session();
    let old = session.attach().unwrap();
    session
        .last_attach_generation
        .store(u64::MAX - 1, Ordering::SeqCst);
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let session = session.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                session.attach_owned()
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 7);
    let current = results.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(current.generation(), u64::MAX);
    assert!(!current.is_displaced());
    assert!(old.is_displaced());
    assert!(session.was_attachment_taken_over(old.generation()));
    drop(old);
    assert!(session.is_attached());
    current.write_raw(b"last winner").unwrap();
    drop(current);
    assert!(!session.is_attached());
}

#[test]
fn owned_inflight_write_finishes_once_and_stale_drop_preserves_borrowed_successor() {
    let (session, _clock, writes, started, release) = clear_input_tests::session(true);
    let owned = session.attach_owned().unwrap();
    let worker = std::thread::spawn(move || owned.write_raw(b"admitted"));
    started.recv_timeout(Duration::from_secs(1)).unwrap();
    let successor = session.attach().unwrap();
    assert_eq!(session.human_inflight.load(Ordering::SeqCst), 1);
    assert!(matches!(
        session.clear_input(b"\x15"),
        Err(AgentError::HumanInputRecent)
    ));
    let (ready, changed) = &*release;
    *ready.lock().unwrap() = true;
    changed.notify_all();
    worker.join().unwrap().unwrap();
    assert!(session.is_attached());
    assert!(!successor.is_displaced());
    assert_eq!(session.human_inflight.load(Ordering::SeqCst), 0);
    successor.write_raw(b"successor").unwrap();
    assert_eq!(
        *writes.lock().unwrap(),
        vec![b"admitted".to_vec(), b"successor".to_vec()]
    );
    drop(successor);
    assert!(!session.is_attached());
}
