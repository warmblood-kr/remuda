//! Experimental isolated host; no automatic/default production adoption.
use remuda_native::session_host;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let operation = args.next().and_then(|value| value.into_string().ok());
    let path = args.next().map(std::path::PathBuf::from);
    // The daemon reads this to pick a host of its own build (#713).
    if operation.as_deref() == Some("version") && path.is_none() {
        println!("{}", remuda_native::dist::BUILD_VERSION);
        return;
    }
    if let Some(refusal) =
        session_host::other_build_refusal(std::env::var_os(session_host::EXPECTED_BUILD).as_deref())
    {
        eprintln!("session host: {refusal}");
        std::process::exit(session_host::BUILD_MISMATCH);
    }
    let result = match (operation, path, args.next()) {
        (Some(operation), Some(path), None) => session_host::executable(&operation, &path),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: remuda-session-host run|inspect|hold|close|stop PRIVATE_DIRECTORY | version",
        )),
    };
    if let Err(error) = result {
        eprintln!("session host: {error}");
        std::process::exit(1);
    }
}
