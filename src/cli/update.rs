use crate::cli::Io;

pub async fn update_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}
