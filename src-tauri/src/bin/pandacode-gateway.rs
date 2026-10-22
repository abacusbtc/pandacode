//! Entry point for `pandacode-gateway`: parse flags, serve, stop on Ctrl-C.
//!
//! Built only with `--features gateway`; see docs/gateway.md.

use pandacode_lib::gateway::{config, spawn};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", config::usage());
        return std::process::ExitCode::SUCCESS;
    }
    let config = match config::from_args(args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match spawn(config).await {
        Ok(handle) => {
            println!("pandacode gateway on http://{}", handle.addr);
            let _ = tokio::signal::ctrl_c().await;
            handle.stop().await;
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot start gateway: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
