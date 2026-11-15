//! Identity checks shared by session clients before reusing a remote selection.

/// Whether a previously selected session is still the same unique instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionIdentity {
    Same,
    ChangedOrMissing,
    Ambiguous,
}

/// One snapshot: `index` is the first exact match, even with a missing instance.
/// `identity` is absent when either prior field is absent. Neither field
/// grants permission to operate on a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionReconciliation {
    pub index: Option<usize>,
    pub identity: Option<SelectionIdentity>,
}

/// Reconcile one prior selection with a single node's ordered snapshot.
/// Duplicate names remain ambiguous even when an exact row exists. This
/// classifies a snapshot; it neither validates trust nor pins later actions.
pub fn reconcile_selection<'a>(
    sessions: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    selected_name: Option<&str>,
    selected_instance: Option<&str>,
) -> SelectionReconciliation {
    let Some(selected_name) = selected_name else {
        return SelectionReconciliation {
            index: None,
            identity: None,
        };
    };
    let mut index = None;
    let mut names = 0usize;
    for (i, (name, instance)) in sessions.into_iter().enumerate() {
        if name == selected_name {
            names += 1;
            if index.is_none() && instance == selected_instance {
                index = Some(i);
            }
        }
    }
    let identity = selected_instance.map(|_| {
        if names > 1 {
            SelectionIdentity::Ambiguous
        } else if index.is_some() {
            SelectionIdentity::Same
        } else {
            SelectionIdentity::ChangedOrMissing
        }
    });
    SelectionReconciliation { index, identity }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_selection<'a>(
        sessions: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
        name: &str,
        instance: &str,
    ) -> SelectionIdentity {
        reconcile_selection(sessions, Some(name), Some(instance))
            .identity
            .unwrap()
    }

    #[test]
    fn preserves_only_the_same_unique_instance() {
        let selected = [("other", Some("x")), ("shell", Some("one"))];
        assert_eq!(
            classify_selection(selected, "shell", "one"),
            SelectionIdentity::Same
        );
        assert_eq!(
            classify_selection(selected, "shell", "two"),
            SelectionIdentity::ChangedOrMissing
        );
        assert_eq!(
            classify_selection(selected, "missing", "one"),
            SelectionIdentity::ChangedOrMissing
        );
    }

    #[test]
    fn duplicate_names_and_unknown_instance_never_match() {
        let duplicates = [("shell", Some("one")), ("shell", Some("two"))];
        assert_eq!(
            classify_selection(duplicates, "shell", "one"),
            SelectionIdentity::Ambiguous
        );
        assert_eq!(
            classify_selection([("shell", None)], "shell", "one"),
            SelectionIdentity::ChangedOrMissing
        );
    }
    #[test]
    fn reconcile_retains_first_exact_index_and_classifies_duplicates() {
        let rows = [
            ("else", Some("e")),
            ("shell", Some("old")),
            ("shell", Some("old")),
        ];
        assert_eq!(
            reconcile_selection(rows, Some("shell"), Some("old")),
            SelectionReconciliation {
                index: Some(1),
                identity: Some(SelectionIdentity::Ambiguous)
            }
        );
        assert_eq!(
            reconcile_selection(rows, Some("shell"), Some("new")),
            SelectionReconciliation {
                index: None,
                identity: Some(SelectionIdentity::Ambiguous)
            }
        );
    }

    #[test]
    fn absent_previous_fields_keep_existing_selection_semantics() {
        let rows = [("other", None), ("shell", None), ("shell", Some("one"))];
        assert_eq!(
            reconcile_selection(rows, None, Some("one")),
            SelectionReconciliation {
                index: None,
                identity: None
            }
        );
        assert_eq!(
            reconcile_selection(rows, Some("shell"), None),
            SelectionReconciliation {
                index: Some(1),
                identity: None
            }
        );
        assert_eq!(
            reconcile_selection(rows, Some("missing"), None),
            SelectionReconciliation {
                index: None,
                identity: None
            }
        );
        assert_eq!(
            reconcile_selection([("shell", Some(""))], Some("shell"), Some("")),
            SelectionReconciliation {
                index: Some(0),
                identity: Some(SelectionIdentity::Same)
            }
        );
    }
}
