use crate::cli::Io;

pub async fn forecast_cmd(io: &mut Io<'_>, _args: &[String]) -> Result<(), String> {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    Err("command not ported yet".to_string())
}
