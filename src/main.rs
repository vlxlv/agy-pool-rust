fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("-v" | "--version" | "version") => println!("agy-pool {}", agy_pool::VERSION),
        _ => {
            eprintln!("agy-pool (rust): only --version is implemented yet");
            std::process::exit(2);
        }
    }
}
