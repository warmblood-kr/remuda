//! A session's processes on Windows, held together by a job object. A parent
//! PID stops naming a session once the parent has exited; a job does not: a
//! process a member starts is in the job, stays in it when its parent exits,
//! and cannot leave it (the job sets no breakaway limit). The decisions are
//! plain functions, tested on every platform; the Windows calls are below them.

use std::io;

/// One row of the system's process list. Times are Windows file times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed {
    pub pid: u32,
    pub parent: u32,
    pub created: u64,
}

/// The one line for a session that could not be put in its job. Such a
/// session is not started: without the job its orphans could not be told
/// from processes outside every session.
pub fn refusal(step: &str, error: &io::Error) -> String {
    format!(
        "the session was not started: its job object could not be {step} ({error}). \
         Next: start remuda from an ordinary terminal, not from inside a restricted job."
    )
}

/// Fail closed: the job, with the child assigned to it, or the child killed
/// and the refusal line.
pub fn set_up<J>(
    job: io::Result<J>,
    assign: impl FnOnce(&J) -> io::Result<()>,
    kill: impl FnOnce(),
) -> Result<J, String> {
    let attempt = job
        .map_err(|error| refusal("created", &error))
        .and_then(|job| match assign(&job) {
            Ok(()) => Ok(job),
            Err(error) => Err(refusal("assigned", &error)),
        });
    if attempt.is_err() {
        kill();
    }
    attempt
}

/// The processes `root` may have started before it was in the job: its
/// children that are not older than it, and theirs. A listed child that is
/// older than its parent has a reused parent PID and is someone else's.
pub fn early_descendants(list: &[Listed], root: u32, root_created: u64) -> Vec<u32> {
    let mut found = Vec::new();
    let mut parents = vec![(root, root_created)];
    while let Some((parent, parent_created)) = parents.pop() {
        for row in list {
            let is_child = row.parent == parent && row.pid != parent;
            if is_child && row.created >= parent_created && !found.contains(&row.pid) {
                found.push(row.pid);
                parents.push((row.pid, row.created));
            }
        }
    }
    found
}

/// Whether the process the daemon opened is the client that connected: it
/// existed before the connection was accepted (else its PID was reused since)
/// and the pipe still names the same PID.
pub fn peer_is_current(created: u64, accepted: u64, pid_before: u32, pid_after: u32) -> bool {
    created < accepted && pid_before == pid_after
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn denied() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "access denied")
    }

    #[test]
    fn a_session_whose_job_cannot_be_assigned_is_killed_and_refused() {
        let killed = Cell::new(false);
        let refused = set_up(Ok(7), |_| Err(denied()), || killed.set(true)).unwrap_err();
        assert!(killed.get(), "the child must not run without its job");
        assert_eq!(
            refused,
            "the session was not started: its job object could not be assigned (access denied). \
             Next: start remuda from an ordinary terminal, not from inside a restricted job."
        );
        assert!(!refused.contains('\n'));
    }

    #[test]
    fn a_job_that_cannot_be_created_refuses_before_any_assignment() {
        let killed = Cell::new(false);
        let assigned = Cell::new(false);
        let refused = set_up::<u8>(
            Err(denied()),
            |_| {
                assigned.set(true);
                Ok(())
            },
            || killed.set(true),
        )
        .unwrap_err();
        assert!(killed.get() && !assigned.get());
        assert!(refused.contains("could not be created"), "{refused}");
    }

    #[test]
    fn a_job_that_is_assigned_is_kept_and_nothing_is_killed() {
        let killed = Cell::new(false);
        assert_eq!(set_up(Ok(7), |_| Ok(()), || killed.set(true)), Ok(7));
        assert!(!killed.get());
    }

    #[test]
    fn early_descendants_are_the_younger_children_and_theirs() {
        let row = |pid, parent, created| Listed {
            pid,
            parent,
            created,
        };
        let list = [
            row(10, 1, 100),  // the session child itself
            row(11, 10, 105), // its child
            row(12, 11, 110), // its grandchild
            row(13, 10, 100), // a child from the same clock tick
            row(20, 10, 50),  // older than its "parent": a reused PID
            row(21, 20, 120), // so its children are not ours either
            row(30, 2, 130),  // unrelated
        ];
        let mut found = early_descendants(&list, 10, 100);
        found.sort_unstable();
        assert_eq!(found, [11, 12, 13]);
        assert!(early_descendants(&list, 99, 0).is_empty());
    }

    #[test]
    fn early_descendants_end_on_a_parent_cycle() {
        let list = [
            Listed {
                pid: 10,
                parent: 11,
                created: 100,
            },
            Listed {
                pid: 11,
                parent: 10,
                created: 100,
            },
        ];
        assert_eq!(early_descendants(&list, 10, 100), [11, 10]);
    }

    #[test]
    fn a_peer_is_current_only_if_it_is_older_than_the_connection() {
        for (created, accepted, before, after, current) in [
            (100, 200, 40, 40, true),
            // Created after the accept: the PID was reused since.
            (300, 200, 40, 40, false),
            // The same tick is not "earlier".
            (200, 200, 40, 40, false),
            // The pipe names another PID now.
            (100, 200, 40, 41, false),
        ] {
            let got = peer_is_current(created, accepted, before, after);
            assert_eq!(got, current, "{created} {accepted} {before} {after}");
        }
    }
}
