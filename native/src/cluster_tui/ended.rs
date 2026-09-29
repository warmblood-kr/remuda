use remuda_core::registry::SessionSummary;
use std::time::Duration;

#[derive(Clone)]
pub struct EndedState {
    pub summary: SessionSummary,
    pub screen: String,
    pub captured_at: Duration,
}

impl EndedState {
    pub fn from_capture(summary: SessionSummary, screen: String, captured_at: Duration) -> Self {
        Self {
            summary,
            screen,
            captured_at,
        }
    }

    pub fn matches(&self, name: &str, instance_id: &str) -> bool {
        self.summary.name == name && self.summary.instance_id.as_deref() == Some(instance_id)
    }

    pub fn ended_summary(&self) -> SessionSummary {
        let mut summary = self.summary.clone();
        summary.alive = false;
        summary.attached = false;
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::EndedState;
    use remuda_core::{SessionSummary, Size};
    use std::time::Duration;

    #[test]
    fn ended_state_keeps_the_last_frame_and_session_identity() {
        let state = EndedState::from_capture(
            SessionSummary {
                id: "dev-id".into(),
                name: "dev".into(),
                instance_id: Some("instance-1".into()),
                output_version: None,
                alive: true,
                idle: Duration::ZERO,
                output_idle: None,
                size: Size::new(80, 24),
                attached: false,
                human_idle: None,
                mouse_tracking: false,
            },
            "last output".into(),
            Duration::from_secs(3),
        );
        assert_eq!(state.screen, "last output");
        assert!(state.matches("dev", "instance-1"));
        assert!(!state.matches("dev", "instance-2"));
        assert!(!state.ended_summary().alive);
    }
}
