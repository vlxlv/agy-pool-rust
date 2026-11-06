use crate::cli::Io;

pub async fn manage_strategy(io: &mut Io<'_>, _target: &str) -> bool {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    false
}

pub async fn status_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}
