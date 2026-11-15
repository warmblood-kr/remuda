//! Headless GUI boundary proof; plain text only, no input or attachment.
use remuda_native::session_preview::{capture, IdentityPolicy};
use remuda_native::tui::SessionPort;

pub fn preview<P: SessionPort + ?Sized>(
    port: &P,
    name: &str,
    instance: &str,
) -> Result<String, String> {
    let frame = capture(port, name, Some(instance), 0, IdentityPolicy::Strict)?;
    let mut text = String::new();
    for row in frame.cells {
        for cell in row {
            for ch in cell.text.chars() {
                if ch.is_control()
                    || remuda_core::protocol::is_secret_prompt_format_or_separator(ch)
                {
                    continue;
                }
                if text.len() + ch.len_utf8() > 65_536 {
                    return Err("Preview exceeds display limits".into());
                }
                text.push(ch);
            }
        }
        if text.len() == 65_536 {
            return Err("Preview exceeds display limits".into());
        }
        text.push('\n');
    }
    Ok(text)
}
