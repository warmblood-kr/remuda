use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

    pub fn prompt(&self, width: usize, help: &str) -> Option<String> {
        self.target.as_ref().map(|(name, _)| {
            let prefix = "kill ";
            let suffix = format!("? {help} · other key cancels");
            let available = width.saturating_sub(UnicodeWidthStr::width(prefix) + 2);
            let name_width = UnicodeWidthStr::width(name.as_str());
            let target = if name_width <= available {
                name.clone()
            } else if available == 0 {
                String::new()
            } else {
                let mut target = String::new();
                let mut used = 0;
                for ch in name.chars() {
                    let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if used + ch_width + 1 > available {
                        break;
                    }
                    target.push(ch);
                    used += ch_width;
                }
                target.push('…');
                target
            };
            format!("{prefix}{target}{suffix}")
        })
    }

    pub fn handle(&mut self, accepted: bool) -> Option<Decision> {
        let (name, instance_id) = self.target.take()?;
        if accepted {
            Some(Decision::Confirmed { name, instance_id })
        } else {
            Some(Decision::Cancelled)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Confirmation, Decision};

    #[test]
    fn confirmation_names_target_and_accepts_only_y() {
        let mut confirmation = Confirmation::default();
        confirmation.begin("dev".into(), "instance-1".into());
        assert_eq!(
            confirmation.prompt(80, "y confirm").as_deref(),
            Some("kill dev? y confirm · other key cancels")
        );
        assert_eq!(
            confirmation.handle(true),
            Some(Decision::Confirmed {
                name: "dev".into(),
                instance_id: "instance-1".into(),
            })
        );

        confirmation.begin("dev".into(), "instance-1".into());
        assert_eq!(confirmation.handle(false), Some(Decision::Cancelled));
    }
}
