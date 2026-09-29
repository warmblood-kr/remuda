use remuda_core::protocol::Request;

pub fn confirmed_close(name: String, instance_id: String) -> Request {
    Request::Close {
        name,
        instance_id: Some(instance_id),
        confirm: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::confirmed_close;
    use remuda_core::protocol::Request;

    #[test]
    fn close_request_carries_confirmation_and_the_selected_instance() {
        assert_eq!(
            confirmed_close("dev".into(), "instance-1".into()),
            Request::Close {
                name: "dev".into(),
                instance_id: Some("instance-1".into()),
                confirm: Some(true),
            }
        );
    }
}
