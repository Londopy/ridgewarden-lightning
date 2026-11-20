use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ridgewarden_client::alerts;
use ridgewarden_lightning::app::{self, App};
use ridgewarden_lightning::config::{Config, EXAMPLE};
use ridgewarden_lightning::sim::{self, Plan, Rng};
use ridgewarden_lightning::{agreement, coverage, receiver, store};
use ridgewarden_lightning::{spawn_loops, web};

#[derive(Parser)]
#[command(
    version,
    about = "Lightning location for AREDN meshes (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/lightning.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core, read the receiver, solve strikes, serve the map
    Run,
    /// Write vttoga-style H records for synthetic strikes, one file per receiver
    Simulate {
        /// a receiver, as name=lat,lon; give three or more
        #[arg(long = "receiver", required = true)]
        receivers: Vec<String>,
        /// a strike position, lat,lon; repeat for more strikes
        #[arg(long = "strike", required = true)]
        strikes: Vec<String>,
        /// directory for <name>.toga files (appended to)
        #[arg(long)]
        out: PathBuf,
        /// timing noise added to each arrival, microseconds (1 sigma)
        #[arg(long, default_value_t = 5.0)]
        sigma_us: f64,
        /// seconds between strikes
        #[arg(long, default_value_t = 2.0)]
        every: f64,
        /// first strike this many seconds from now
        #[arg(long, default_value_t = 0.0)]
        delay: f64,
        #[arg(long, default_value_t = 0.9922)]
        velocity_factor: f64,
        /// seed for the noise; random if not given
        #[arg(long)]
        seed: Option<u64>,
    },
    /// Read the end of the configured vttoga output and say what's in it
    CheckSource,
    /// Compare our strikes with a reference CSV (time,lat,lon) and optionally log the result
    Compare {
        /// the reference file
        #[arg(long)]
        reference: PathBuf,
        /// what the reference is, shown with the result, e.g. "GLM, 2026-11-19 storm"
        #[arg(long)]
        source: String,
        /// how close in time a match must be, milliseconds
        #[arg(long, default_value_t = 50.0)]
        window_ms: f64,
        /// log the result through the core so every site sees it
        #[arg(long)]
        publish: bool,
    },
    /// Send a test notice through the configured alert channels
    TestAlert,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_lightning=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Simulate {
            receivers,
            strikes,
            out,
            sigma_us,
            every,
            delay,
            velocity_factor,
            seed,
        } => {
            let plan = Plan {
                receivers: receivers
                    .iter()
                    .map(|r| sim::parse_receiver(r))
                    .collect::<Result<_>>()?,
                strikes: strikes
                    .iter()
                    .map(|s| sim::parse_point(s))
                    .collect::<Result<_>>()?,
                every_s: every,
                sigma_us,
                v_km_s: velocity_factor * ridgewarden_lightning::solve::C_KM_S,
                start_us: (ridgewarden_client::util::now_ms() as f64 * 1000.0 + delay * 1e6) as i64,
            };
            let seed = match seed {
                Some(s) => s,
                None => getrandom::u64().context("no OS randomness")?,
            };
            let n = sim::write(&plan, &out, &mut Rng::new(seed))?;
            println!("Wrote {n} records to {}", out.display());
            Ok(())
        }
        Cmd::CheckSource => {
            let cfg = Config::load(&cli.config)?;
            let Some(r) = &cfg.receiver else {
                bail!("{} has no [receiver] section", cli.config.display());
            };
            println!(
                "{}",
                receiver::check_source(r, ridgewarden_client::util::now_ms())?
            );
            Ok(())
        }
        Cmd::Compare {
            reference,
            source,
            window_ms,
            publish,
        } => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?
                .block_on(compare(cfg, &reference, &source, window_ms, publish))
        }
        Cmd::TestAlert => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(test_alert(cfg))
        }
    }
}

async fn compare(
    cfg: Config,
    file: &Path,
    source: &str,
    window_ms: f64,
    publish: bool,
) -> Result<()> {
    if !(window_ms > 0.0 && window_ms <= 10_000.0) {
        bail!("--window-ms must be above 0 and at most 10000");
    }
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let (reference, skipped) = agreement::parse(&text);
    if reference.is_empty() {
        bail!(
            "no strikes in {} (lines look like time,lat,lon)",
            file.display()
        );
    }
    if skipped > 0 {
        match skipped {
            1 => println!("Skipped 1 line that isn't time,lat,lon"),
            n => println!("Skipped {n} lines that aren't time,lat,lon"),
        }
    }
    let app = App::new(cfg)?;
    let (ours, ring) = {
        let db = app.db();
        let from = reference.iter().map(|r| r.t_us).min().unwrap_or(0) - 10_000_000;
        let to = reference.iter().map(|r| r.t_us).max().unwrap_or(0) + 10_000_000;
        let ours = store::strikes(&db, from, to, 1_000_000)?;
        let rx = store::receivers(&db, ridgewarden_client::util::now_ms())?;
        let pts: Vec<_> = rx.iter().map(|r| r.pos).collect();
        (ours, coverage::hull(&pts))
    };
    if ring.is_empty() {
        println!(
            "No ring of receivers yet (needs three that reported in the last week); nothing counts as inside"
        );
    }
    let a = agreement::compare(&reference, &ours, &ring, window_ms, source);
    println!("{}", agreement::summary(&a));
    if publish {
        app.core
            .append("lightning.agreement", serde_json::to_value(&a)?)
            .await
            .context("logging the result through the core")?;
        println!("Logged; it shows on every site's Accuracy page.");
    }
    Ok(())
}

async fn test_alert(cfg: Config) -> Result<()> {
    let a = &cfg.alerts;
    if a.ntfy.is_none() && a.discord.is_none() && a.webhooks.is_empty() && a.email.is_none() {
        bail!("no alert channel is configured under [alerts]");
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let n = alerts::Notice {
        key: "test".into(),
        state: "open",
        severity: alerts::Severity::Info,
        title: "Test alert".into(),
        text: "A test from ridgewarden-lightning. If this arrived with the internet unplugged, \
               alerts reach you over the mesh."
            .into(),
        opened_ms: ridgewarden_client::util::now_ms(),
    };
    alerts::send(&http, a, app::NAME, &n, "test").await?;
    println!("Delivered to at least one channel; any that failed are logged above.");
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-lightning {} following {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
