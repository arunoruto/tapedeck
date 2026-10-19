use anyhow::Result;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
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
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,lofty=error")))
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
    // On a terminal, long jobs get a bar (or a spinner); everything else prints above it. In a
    // pipe or a log, progress is dropped and the rest stays plain lines.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut bar: Option<ProgressBar> = None;
    let say = |bar: &Option<ProgressBar>, line: String| match bar {
        Some(b) => b.println(line),
        None => eprintln!("{line}"),
    };
    while let Some(ev) = rx.recv().await {
        if cli.json {
            println!("{}", serde_json::to_string(&ev)?);
            if matches!(ev, Event::Error { .. }) {
                failed = true;
            }
            continue;
        }
        match ev {
            Event::Progress { msg, current, total } if tty => {
                let b = bar.get_or_insert_with(|| {
                    let b = ProgressBar::new_spinner();
                    b.enable_steady_tick(std::time::Duration::from_millis(120));
                    b
                });
                match (current, total) {
                    (Some(cur), Some(len)) if len > 0 => {
                        if b.length() != Some(len) {
                            b.set_style(
                                ProgressStyle::with_template(
                                    "{spinner:.cyan} [{bar:28.cyan/dim}] {pos}/{len} {eta:>4} {wide_msg}",
                                )
                                .expect("template")
                                .progress_chars("━╸─"),
                            );
                            b.set_length(len);
                        }
                        b.set_position(cur.saturating_sub(1));
                    }
                    _ => b.set_style(ProgressStyle::with_template("{spinner:.cyan} {wide_msg}").expect("template")),
                }
                b.set_message(msg);
            }
            Event::Progress { .. } => {}
            Event::Info { msg } => say(&bar, msg),
            Event::Data { data } => {
                let text = tapedeck::render::render(&data);
                match &bar {
                    Some(b) => b.suspend(|| println!("{text}")),
                    None => println!("{text}"),
                }
            }
            Event::Error { msg } => {
                failed = true;
                say(&bar, format!("error: {msg}"));
            }
        }
    }
    if let Some(b) = &bar {
        b.finish_and_clear();
    }
    worker.await??;
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
