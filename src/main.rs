use anyhow::Result;
use clap::Parser;
use tapedeck::{
    Event,
    cli::{Cli, Command},
    config::Config,
    daemon,
};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;

    let cmd = match cli.cmd {
        None | Some(Command::Tui) => return tapedeck::tui::run(config).await,
        Some(Command::Daemon) => return daemon::serve(config).await,
        Some(cmd) => cmd,
    };

    let (tx, mut rx) = mpsc::channel(64);
    let worker = tokio::spawn(async move { daemon::dispatch(&config, cmd, tx).await });

    let mut failed = false;
    // Progress lines overwrite each other on a terminal and are dropped from logs/pipes.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut progressing = false;
    while let Some(ev) = rx.recv().await {
        if cli.json {
            println!("{}", serde_json::to_string(&ev)?);
        } else if progressing && !matches!(ev, Event::Progress { .. }) {
            eprint!("\r\x1b[2K");
            progressing = false;
        }
        match ev {
            Event::Progress { msg } if !cli.json && tty => {
                eprint!("\r\x1b[2K{msg}");
                progressing = true;
            }
            Event::Info { msg } if !cli.json => eprintln!("{msg}"),
            Event::Data { data } if !cli.json => println!("{}", tapedeck::render::render(&data)),
            Event::Error { msg } => {
                failed = true;
                if !cli.json {
                    eprintln!("error: {msg}");
                }
            }
            _ => {}
        }
    }
    if progressing {
        eprintln!();
    }
    worker.await??;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
