//! The running app: its config, its database, and its line to the core.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;

use crate::config::Config;
use crate::rain::WeatherFeed;
use crate::store::Store;

pub const PROJECTIONS: &[&dyn Projection] = &[&Store, &WeatherFeed];
pub const NAME: &str = "ridgewarden-lightning";

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// for alert delivery, which may go out to the internet over TLS
    pub alert_http: reqwest::Client,
    /// the core's identity, cached once it knows its node
    who: Mutex<Option<Whoami>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let token = cfg
            .core
            .token_file
            .exists()
            .then_some(cfg.core.token_file.as_path());
        if token.is_none() && (cfg.receiver.is_some() || cfg.solver.enabled) {
            tracing::warn!(
                "core token {} not found; detections and strikes can't be logged",
                cfg.core.token_file.display()
            );
        }
        let core = CoreClient::new(&cfg.core.url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(ridgewarden_client::alerts::SCHEMA)?;
        db.execute_batch(crate::locate::SCHEMA)?;
        db.execute_batch(crate::rain::SCHEMA)?;
        Ok(Self {
            cfg,
            core,
            db: Mutex::new(db),
            alert_http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("ridgewarden-lightning/", env!("CARGO_PKG_VERSION")))
                .build()?,
            who: Mutex::new(None),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The core's identity; the node is "" until the core has polled it.
    pub async fn whoami(&self) -> Result<Whoami> {
        if let Some(w) = self.who.lock().unwrap().clone()
            && !w.node.is_empty()
        {
            return Ok(w);
        }
        let w = self.core.whoami().await?;
        *self.who.lock().unwrap() = Some(w.clone());
        Ok(w)
    }
}
