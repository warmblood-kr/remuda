use std::io::{Read, Write};

fn main() {
    println!("{}", std::env::current_exe().unwrap().display());
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    println!("{}", std::env::current_exe().unwrap().display());
}
