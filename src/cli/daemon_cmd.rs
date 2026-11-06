use crate::cli::Io;

pub async fn handle_logs(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn start_daemon_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn stop_daemon_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn restart_daemon_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}
