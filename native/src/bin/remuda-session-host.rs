//! Experimental isolated host; no automatic/default production adoption.
fn main() {
    let mut args = std::env::args_os().skip(1);
    let operation = args.next().and_then(|value| value.into_string().ok());
    let path = args.next().map(std::path::PathBuf::from);
    let result = match (operation, path, args.next()) {
        (Some(operation), Some(path), None) => {
            remuda_native::session_host::executable(&operation, &path)
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: remuda-session-host run|inspect|hold|close|stop PRIVATE_DIRECTORY",
        )),
    };
    if let Err(error) = result {
        eprintln!("session host: {error}");
        std::process::exit(1);
    }
}
