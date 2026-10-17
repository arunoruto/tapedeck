use anyhow::Result;
use clap::Parser;
use tapedeck::{
    App, Event,
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
    let worker = match daemon::connect(&config).await {
        Some(stream) => tokio::spawn(daemon::forward(stream, cmd, tx)),
        None => {
            let app = App::open(config)?;
            tokio::spawn(async move {
                app.run(cmd, tx).await;
                Ok(())
            })
        }
    };

    let mut failed = false;
    while let Some(ev) = rx.recv().await {
        if cli.json {
            println!("{}", serde_json::to_string(&ev)?);
        }
        match ev {
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
    worker.await??;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
