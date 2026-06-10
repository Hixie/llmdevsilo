use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let connect = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SILO_HELPER_CONNECT").ok());
    let Some(connect) = connect else {
        eprintln!("usage: silo-helper <unix:PATH | tcp:HOST:PORT>");
        return ExitCode::from(2);
    };
    match silo_helper::run(&connect).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("silo-helper: {message}");
            ExitCode::FAILURE
        }
    }
}
