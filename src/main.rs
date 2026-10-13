use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use rowboat::app::{App, Msg};
use rowboat::config::{self, Config};
use rowboat::db::{Session, meta};
use rowboat::{op, ui};

/// A read-only, keyboard-driven database viewer.
///
/// Connections are listed in a config file and their details are read from
/// 1Password when you connect. rowboat only reads: statements that could
/// change anything are refused.
#[derive(Parser)]
#[command(name = "rowboat", version)]
struct Cli {
    /// Open this connection straight away
    connection: Option<String>,

    /// Use this config file instead of the default
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// List the configured connections and exit
    #[arg(long)]
    list: bool,

    /// Resolve and test a connection without opening the interface
    #[arg(long, value_name = "NAME")]
    check: Option<String>,

    /// Print an example config file and exit
    #[arg(long)]
    example_config: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.example_config {
        print!("{}", config::EXAMPLE);
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Database drivers build deep futures; give them room.
        .thread_stack_size(8 * 1024 * 1024)
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("rowboat: could not start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli));
    // Do not wait for a hung DNS lookup or a query nobody is waiting for.
    runtime.shutdown_background();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rowboat: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let explicit = cli.config.is_some();
    let path = cli.config.clone().unwrap_or_else(config::default_path);

    // A missing default config is the normal first run, not an error.
    let loaded = if path.exists() || explicit {
        config::load(&path)
    } else {
        Ok(Config::default())
    };

    if cli.list {
        return list(&loaded?, &path);
    }
    if let Some(name) = &cli.check {
        return check(&loaded?, name).await;
    }

    // In the interface a broken config is shown rather than fatal, so it can
    // be fixed and reloaded with r.
    let (cfg, cfg_error) = match loaded {
        Ok(cfg) => (cfg, None),
        Err(e) => (Config::default(), Some(format!("{e:#}"))),
    };
    interface(cfg, path, cfg_error, cli.connection).await
}

fn list(cfg: &Config, path: &std::path::Path) -> Result<()> {
    if cfg.connections.is_empty() {
        println!("No connections in {}", path.display());
        println!("Run `rowboat --example-config` to see what one looks like.");
        return Ok(());
    }
    let width = cfg
        .connections
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    for connection in &cfg.connections {
        println!(
            "{:width$}  {:9}  {}",
            connection.name,
            connection.engine.as_deref().unwrap_or("-"),
            connection.summary()
        );
    }
    Ok(())
}

async fn check(cfg: &Config, name: &str) -> Result<()> {
    let connection = cfg
        .connections
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow!("There is no connection called \"{name}\""))?;

    println!("{}", connection.name);
    let spec = op::resolve(connection, &cfg.settings)
        .await
        .context("Could not work out the connection details")?;
    println!("  engine    {}", spec.engine.label());
    println!("  target    {}", spec.target());
    println!("  user      {}", spec.username.as_deref().unwrap_or("-"));
    println!(
        "  password  {}",
        if spec.password.is_some() {
            "found (not shown)"
        } else {
            "none"
        }
    );

    let session = Session::connect(spec, cfg.settings.query_timeout())
        .await
        .context("Could not connect")?;
    let listing = session
        .catalog_query(
            meta::objects_sql(session.engine()),
            cfg.settings.custom_limits(),
        )
        .await
        .context("Connected, but could not list tables")?;
    println!(
        "  connected read-only: {} tables and views visible",
        meta::parse_objects(&listing).len()
    );
    Ok(())
}

async fn interface(
    cfg: Config,
    path: PathBuf,
    cfg_error: Option<String>,
    connection: Option<String>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(cfg, path, cfg_error, tx.clone());
    if let Some(name) = connection {
        app.connect_by_name(&name);
    }

    let mut terminal = ratatui::try_init().context("Could not take over the terminal")?;
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);

    // Keys are read on their own thread so a slow redraw never drops one.
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if tx.send(Msg::Term(event)).is_err() {
                break;
            }
        }
    });

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let outcome = loop {
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            break Err(anyhow!("Could not draw: {e}"));
        }
        tokio::select! {
            Some(msg) = rx.recv() => app.handle(msg),
            _ = ticker.tick() => app.on_tick(),
        }
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
        }
        if app.quit {
            break Ok(());
        }
    };

    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    outcome
}
