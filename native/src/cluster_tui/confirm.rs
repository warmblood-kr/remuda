use crossterm::event::KeyCode;

#[derive(Default)]
pub struct Confirmation {
    target: Option<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Confirmed { name: String, instance_id: String },
    Cancelled,
}

impl Confirmation {
    pub fn begin(&mut self, name: String, instance_id: String) {
        self.target = Some((name, instance_id));
    }

    pub fn prompt(&self) -> Option<String> {
        self.target
            .as_ref()
            .map(|(name, _)| format!("kill {name}? it is running — y / n"))
    }

    pub fn handle(&mut self, key: KeyCode) -> Option<Decision> {
        let (name, instance_id) = self.target.take()?;
        if matches!(key, KeyCode::Char('y' | 'Y')) {
            Some(Decision::Confirmed { name, instance_id })
        } else {
            Some(Decision::Cancelled)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Confirmation, Decision};
    use crossterm::event::KeyCode;

    #[test]
    fn confirmation_names_target_and_accepts_only_y() {
        let mut confirmation = Confirmation::default();
        confirmation.begin("dev".into(), "instance-1".into());
        assert_eq!(
            confirmation.prompt().as_deref(),
            Some("kill dev? it is running — y / n")
        );
        assert_eq!(
            confirmation.handle(KeyCode::Char('y')),
            Some(Decision::Confirmed {
                name: "dev".into(),
                instance_id: "instance-1".into(),
            })
        );

        confirmation.begin("dev".into(), "instance-1".into());
        assert_eq!(
            confirmation.handle(KeyCode::Char('n')),
            Some(Decision::Cancelled)
        );
    }
}
