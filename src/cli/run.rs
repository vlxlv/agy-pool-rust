use crate::cli::Io;

pub async fn run_agy_with_lb(io: &mut Io<'_>, _extra: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn run_agy_direct(io: &mut Io<'_>, _extra: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}
