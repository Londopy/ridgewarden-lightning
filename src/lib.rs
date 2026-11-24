//! ridgewarden-lightning: lightning location over an AREDN mesh, with or without the
//! internet.
//!
//! Each site with a VLF receiver runs vlfrx-tools' `vttoga`, which writes the GPS-timed
//! arrival time of every sferic (a lightning strike's radio pulse). This app reads that
//! output and logs the arrivals through the site's ridgewarden core. A site running the
//! solver collects every site's arrivals from the feed, groups the ones that fit one
//! strike, locates it with an error ellipse, and logs the strikes, so every site can
//! show the map. Strikes inside a watch area raise alerts, and a dry-lightning alert when
//! a rain gauge says under 2.5 mm has fallen.

pub mod accuracy;
pub mod agreement;
pub mod app;
pub mod config;
pub mod coverage;
pub mod geo;
pub mod locate;
pub mod map;
pub mod rain;
pub mod receiver;
pub mod sim;
pub mod solve;
pub mod store;
#[cfg(test)]
mod testing;
pub mod toga;
pub mod top;
pub mod watch;
pub mod web;

use std::sync::Arc;
use std::time::Duration;

use ridgewarden_client::util::now_ms;

async fn alert_round(app: &app::App) -> anyhow::Result<usize> {
    let active = watch::status(&app.db(), &app.cfg, now_ms())?.due;
    ridgewarden_client::alerts::run(
        &app.alert_http,
        &app.db,
        &app.cfg.alerts,
        app::NAME,
        active,
        &|_| false,
    )
    .await
}

/// Follow the core's feed, read the receiver, solve, and alert, forever.
pub fn spawn_loops(app: Arc<app::App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = ridgewarden_client::follow(&a.core, &a.db, app::PROJECTIONS).await {
                tracing::warn!("following the core: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    tokio::spawn(receiver::run(app.clone()));
    tokio::spawn(rain::run(app.clone()));
    if app.cfg.solver.enabled {
        let a = app.clone();
        tokio::spawn(async move {
            let mut turned = locate::Turned::default();
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                match locate::round(&a, &mut turned).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("located {n} strikes"),
                    Err(e) => tracing::warn!("solver: {e:#}"),
                }
            }
        });
    }
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            if let Err(e) = store::prune(&a.db(), now_ms()) {
                tracing::warn!("pruning old arrivals: {e:#}");
            }
        }
    });
    if !app.cfg.watch.is_empty() {
        tokio::spawn(async move {
            // let the feed catch up first, or every old strike looks new
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                match alert_round(&app).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("{n} alert notices sent"),
                    Err(e) => tracing::warn!("alerts: {e:#}"),
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }
}
