use crate::cli::Io;

pub async fn list_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn quota_cmd(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn rename_account(io: &mut Io<'_>, _target: &str, _name: &str) -> bool {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    false
}

pub async fn remove_account(io: &mut Io<'_>, _target: &str) -> bool {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    false
}

pub async fn disable_account(io: &mut Io<'_>, _target: &str) -> bool {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    false
}

pub async fn enable_account(io: &mut Io<'_>, _target: &str) -> bool {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    false
}

pub async fn switch_account(io: &mut Io<'_>, _target: &str) -> bool {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    false
}

pub async fn handle_export(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn handle_import(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn handle_migrate_legacy(io: &mut Io<'_>, _args: &[String]) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}

pub async fn login_cmd(io: &mut Io<'_>) -> i32 {
    let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet");
    2
}
