//! `ridgewarden-lightning top`: the latest strikes, the watch areas and the receivers, live
//! in a terminal, for an operator on a headless Pi over SSH. It reads this app's own JSON
//! (`/api/strikes`, `/api/areas`, `/api/receivers`, and `/api/strike` for one strike's
//! arrivals), on this box or at another site.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use ridgewarden_client::tui::ratatui::Frame;
use ridgewarden_client::tui::ratatui::buffer::{Buffer, Cell as Spot};
use ridgewarden_client::tui::ratatui::crossterm::event::{KeyCode, KeyEvent};
use ridgewarden_client::tui::ratatui::layout::{Constraint, Layout, Rect};
use ridgewarden_client::tui::ratatui::style::{Modifier, Style};
use ridgewarden_client::tui::ratatui::text::{Line, Span};
use ridgewarden_client::tui::ratatui::widgets::{
    Cell, Paragraph, Row, Table, TableState, Widget, Wrap,
};
use ridgewarden_client::tui::{
    self, Chrome, Fetch, Notice, Pill, Refresh, Theme, View, move_key, pill_table, tab_key,
};
use ridgewarden_client::ui::Color;
use ridgewarden_client::util::{human_duration, now_ms, short_time};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::coverage;
use crate::geo::LatLon;

const TABS: [&str; 3] = ["Strikes", "Areas", "Receivers"];
const HELP: &str = "q quit  1-3 Tab pages  Up Down PgUp PgDn move  Enter details  r reload";
const BACK: &str = "q quit  Esc back  Up Down the next one  1-3 Tab pages  r reload";
/// Strikes read before the first draw says how many the screen holds.
const FIRST: usize = 50;
/// The most strikes the app lists at once.
const MOST: usize = 1000;
/// Receiver status takes a week of strikes to work out, so it is read this often, not
/// every round.
const SLOW_MS: i64 = 30_000;

#[derive(Deserialize, Clone, Debug)]
struct Strikes {
    generated: i64,
    hours: i64,
    total: usize,
    solver: Solver,
    strikes: Vec<StrikeRow>,
}

#[derive(Deserialize, Clone, Debug)]
struct Solver {
    enabled: bool,
    min_sites: usize,
}

#[derive(Deserialize, Clone, Debug)]
struct StrikeRow {
    id: String,
    time: String,
    t_us: i64,
    lat: f64,
    lon: f64,
    major_km: f64,
    minor_km: f64,
    az_deg: f64,
    rms_us: Option<f64>,
    ambiguous: bool,
    unchecked: bool,
    areas: Vec<String>,
    site_count: usize,
    bearings_in_fit: usize,
}

/// A strike's arrivals, read while its details are open.
#[derive(Deserialize, Clone, Debug)]
struct Arrivals {
    id: String,
    sites: Vec<ArrivalRow>,
}

#[derive(Deserialize, Clone, Debug)]
struct ArrivalRow {
    label: String,
    t_us: i64,
    resid_us: f64,
    impulse: Option<f64>,
    bearing_deg: Option<f64>,
    bearing_resid_deg: Option<f64>,
    bearing_left_out: bool,
}

#[derive(Deserialize, Clone, Debug)]
struct Areas {
    window_min: u64,
    areas: Vec<AreaRow>,
}

#[derive(Deserialize, Clone, Debug)]
struct AreaRow {
    name: String,
    site_label: Option<String>,
    center: Option<[f64; 2]>,
    radius_km: Option<f64>,
    corners: usize,
    strikes: usize,
    capped: bool,
    latest_us: Option<i64>,
    /// the alert the app's rules call for now: dry, lightning, quiet, or waiting for its
    /// site's position
    state: String,
    rain: Rain,
    alert: Option<OpenAlert>,
}

#[derive(Deserialize, Clone, Debug)]
struct Rain {
    verdict: String,
    mm: Option<f64>,
    text: String,
}

#[derive(Deserialize, Clone, Debug)]
struct OpenAlert {
    severity: String,
    title: String,
    text: String,
    opened_ms: i64,
    acked: bool,
}

#[derive(Deserialize, Clone, Debug)]
struct Receivers {
    receivers: Vec<ReceiverRow>,
}

#[derive(Deserialize, Clone, Debug)]
struct ReceiverRow {
    site: String,
    label: String,
    lat: f64,
    lon: f64,
    sigma_us: f64,
    gps: String,
    state: String,
    color: String,
    last_report_ms: i64,
    last_sferic_us: Option<i64>,
    sferics_last_hour: i64,
    dropped_last_hour: i64,
    error: Option<String>,
    clock_s: Option<f64>,
    bearing_sigma_deg: Option<f64>,
    strikes_7d: usize,
    median_residual_us: Option<f64>,
    median_bearing_residual_deg: Option<f64>,
    bearings_left_out: bool,
}

impl Receivers {
    /// The ring the receivers make: their convex hull.
    fn ring(&self) -> Vec<LatLon> {
        let points: Vec<LatLon> = self
            .receivers
            .iter()
            .map(|r| LatLon::new(r.lat, r.lon))
            .collect();
        coverage::hull(&points)
    }
}

/// Why a read of the app failed.
#[derive(Clone, Debug, PartialEq)]
struct Failed {
    /// what was read, e.g. "/api/areas"
    path: String,
    /// in `Fetch`'s few words: "could not connect", "timed out", "HTTP 404"
    why: String,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// nothing answered: not running, or the wrong address
    Unreachable,
    /// something answered, but not with what this version reads
    OtherFormat,
    /// an HTTP error, such as a 500
    Other,
}

impl Failed {
    fn of(path: &str, url: &str, e: &anyhow::Error) -> Self {
        let msg = e.to_string();
        let why = msg.strip_prefix(&format!("{url}: ")).unwrap_or(&msg);
        // anything short of an answer counts as no answer: a refused connection, a timeout,
        // or a pooled connection to an app that has since stopped
        let kind = match e.downcast_ref::<reqwest::Error>() {
            Some(r) if r.is_decode() || r.status().is_some_and(|s| s.as_u16() == 404) => {
                Kind::OtherFormat
            }
            Some(r) if r.status().is_some() => Kind::Other,
            _ => Kind::Unreachable,
        };
        Self {
            path: path.into(),
            why: why.into(),
            kind,
        }
    }

    /// What can be said about it beyond the reason.
    fn hint(&self) -> &'static str {
        match self.kind {
            Kind::Unreachable => {
                "Is ridgewarden-lightning running there? On this box, start it with: sudo systemctl start ridgewarden-lightning."
            }
            Kind::OtherFormat => {
                "It answered in another version's format: is it ridgewarden-lightning, and the same version as this one?"
            }
            Kind::Other => "",
        }
    }
}

/// One read of the app.
struct Snap {
    /// when it was read, by this machine's clock
    at: i64,
    strikes: Strikes,
    areas: Areas,
    receivers: Receivers,
    /// the ring of the receivers that reported this week
    ring: Vec<LatLon>,
    /// the arrivals of the strike whose details are open, once read
    arrivals: Option<Arrivals>,
}

impl Snap {
    /// The app's clock now, so strikes from a site whose clock differs from ours don't
    /// come out minutes old or in the future.
    fn app_now(&self, now: i64) -> i64 {
        self.strikes.generated + (now - self.at)
    }

    /// What tells each row of a page apart, in order.
    fn keys(&self, tab: usize) -> Vec<&str> {
        match tab {
            0 => self.strikes.strikes.iter().map(|s| s.id.as_str()).collect(),
            1 => self.areas.areas.iter().map(|a| a.name.as_str()).collect(),
            _ => self
                .receivers
                .receivers
                .iter()
                .map(|r| r.site.as_str())
                .collect(),
        }
    }

    fn outside(&self, s: &StrikeRow) -> bool {
        coverage::outside(LatLon::new(s.lat, s.lon), &self.ring)
    }
}

/// What the screen wants read, beyond each round's usual.
struct Asks {
    /// strikes to list: as far as a page past the selection
    rows: AtomicUsize,
    /// the strike whose details are open
    strike: Mutex<Option<String>>,
    /// read receiver status now rather than when it's due
    receivers: AtomicBool,
}

impl Default for Asks {
    fn default() -> Self {
        Self {
            rows: AtomicUsize::new(FIRST),
            strike: Mutex::new(None),
            receivers: AtomicBool::new(false),
        }
    }
}

struct Loader {
    fetch: Fetch,
    base: String,
    asks: Arc<Asks>,
    /// the last receiver status read, and when
    receivers: Option<(i64, Receivers)>,
    /// the last arrivals read; a strike's don't change once it's logged
    arrivals: Option<Arrivals>,
}

impl Loader {
    async fn load(&mut self) -> Result<Snap, Failed> {
        let at = now_ms();
        let rows = self.asks.rows.load(Ordering::Relaxed).clamp(1, MOST);
        let strikes_path = format!("/api/strikes?limit={rows}");
        let now_please = self.asks.receivers.swap(false, Ordering::Relaxed);
        let cached = self
            .receivers
            .take()
            .filter(|(t, _)| !now_please && at - t < SLOW_MS);
        let open = self.asks.strike.lock().unwrap().clone();
        let have = self
            .arrivals
            .take()
            .filter(|a| Some(&a.id) == open.as_ref());
        let (strikes, areas, receivers, arrivals) = tokio::join!(
            self.get::<Strikes>(&strikes_path),
            self.get::<Areas>("/api/areas"),
            async {
                match cached {
                    Some(c) => Ok(c),
                    None => self.get("/api/receivers").await.map(|r| (at, r)),
                }
            },
            async {
                match (have, &open) {
                    (Some(a), _) => Ok(Some(a)),
                    (None, Some(id)) => self.get(&format!("/api/strike?id={id}")).await,
                    (None, None) => Ok(None),
                }
            },
        );
        let (strikes, areas) = (strikes?, areas?);
        let ((read_at, receivers), arrivals) = (receivers?, arrivals?);
        self.receivers = Some((read_at, receivers.clone()));
        self.arrivals.clone_from(&arrivals);
        Ok(Snap {
            at,
            strikes,
            areas,
            ring: receivers.ring(),
            receivers,
            arrivals,
        })
    }

    /// GET `path` from the app and read its JSON.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failed> {
        let url = format!("{}{path}", self.base);
        self.fetch
            .json(&url)
            .await
            .map_err(|e| Failed::of(path, &url, &e))
    }
}

/// Run `top` against the app at `url` until the user quits, reading it again every
/// `every`.
pub fn run(url: &str, every: Duration) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("--url is the app's address, like http://127.0.0.1:8796");
    }
    let base = url.trim_end_matches('/').to_string();
    let asks = Arc::new(Asks::default());
    let mut loader = Loader {
        fetch: Fetch::new(Duration::from_secs(5))?,
        base: base.clone(),
        asks: asks.clone(),
        receivers: None,
        arrivals: None,
    };
    let mut top = Top {
        refresh: Refresh::spawn(every, async move || loader.load().await)?,
        asks,
        base,
        theme: Theme::from_env(),
        snap: None,
        err: None,
        tab: 0,
        rows: Default::default(),
        detail: false,
        page: 0,
    };
    tui::run(&mut top, Duration::from_millis(250))?;
    Ok(())
}

struct Top {
    base: String,
    theme: Theme,
    refresh: Refresh<Result<Snap, Failed>>,
    asks: Arc<Asks>,
    snap: Option<Snap>,
    /// why the latest read failed, if it did; the last good one stays on screen
    err: Option<Failed>,
    tab: usize,
    /// each page's selected row
    rows: [TableState; TABS.len()],
    /// showing the selected row's details instead of the list
    detail: bool,
    /// rows the page's table had room for at the last draw, for PgUp and PgDn
    page: usize,
}

impl Top {
    /// Take a read, keeping each page's selection on the same strike, area or receiver:
    /// new strikes arrive at the top of the list.
    fn take(&mut self, read: Result<Snap, Failed>) {
        let new = match read {
            Ok(s) => s,
            Err(e) => {
                self.err = Some(e);
                return;
            }
        };
        for (tab, rows) in self.rows.iter_mut().enumerate() {
            let picked = self
                .snap
                .as_ref()
                .zip(rows.selected())
                .and_then(|(old, i)| old.keys(tab).get(i).map(|k| k.to_string()));
            if let Some(k) = picked {
                let at = new.keys(tab).iter().position(|x| *x == k);
                rows.select(at);
                if at.is_none() && tab == self.tab {
                    self.detail = false;
                }
            }
        }
        self.snap = Some(new);
        self.err = None;
    }

    /// Tell the loader what the screen needs: the strikes as far as a page past the
    /// selection, and the arrivals of the strike whose details are open. True when that
    /// can't wait for the next round.
    fn ask(&self) -> bool {
        let page = self.page.max(1);
        let picked = self.rows[0].selected();
        self.asks
            .rows
            .store(picked.unwrap_or(0) + 2 * page, Ordering::Relaxed);
        let Some(s) = &self.snap else {
            return false;
        };
        let listed = s.strikes.strikes.len();
        let open = picked
            .filter(|_| self.detail && self.tab == 0)
            .and_then(|i| s.strikes.strikes.get(i))
            .map(|x| x.id.clone());
        let unread = open
            .as_ref()
            .is_some_and(|id| s.arrivals.as_ref().is_none_or(|a| a.id != *id));
        let near_end = picked.is_some_and(|i| i + page >= listed) && listed < s.strikes.total;
        *self.asks.strike.lock().unwrap() = open;
        unread || near_end
    }

    fn status(&self, now: i64) -> Span<'static> {
        let trouble = self.err.as_ref().map(|e| match e.kind {
            Kind::Unreachable => "app not answering",
            _ => "read failed",
        });
        let Some(s) = &self.snap else {
            return match trouble {
                Some(t) => Span::styled(t, self.theme.bad()),
                None => Span::styled("loading", self.theme.mute()),
            };
        };
        let age = format!("updated {} ago", human_duration(now - s.at));
        match trouble {
            Some(t) => Span::styled(format!("{t}, {age}"), self.theme.bad()),
            None => Span::styled(age, self.theme.mute()),
        }
    }
}

impl View for Top {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(read) = self.refresh.latest()? {
            self.take(read);
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        let len = self.snap.as_ref().map_or(0, |s| s.keys(self.tab).len());
        if let Some(t) = tab_key(&k, self.tab, TABS.len()) {
            self.tab = t;
            self.detail = false;
        } else {
            let rows = &mut self.rows[self.tab];
            match k.code {
                KeyCode::Char('r') => {
                    self.asks.receivers.store(true, Ordering::Relaxed);
                    self.refresh.again();
                }
                KeyCode::Esc | KeyCode::Backspace => self.detail = false,
                KeyCode::Enter if self.detail => self.detail = false,
                KeyCode::Enter if len > 0 => {
                    if rows.selected().is_none() {
                        rows.select(Some(0));
                    }
                    self.detail = true;
                }
                _ => {
                    move_key(rows, &k, len, self.page);
                }
            }
        }
        if self.ask() {
            self.refresh.again();
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        let now = now_ms();
        let host = self.base.split_once("://").map_or(&*self.base, |(_, h)| h);
        let title = format!("ridgewarden-lightning top  {host}");
        let chrome = Chrome {
            title: &title,
            status: self.status(now),
            tabs: &TABS,
            tab: self.tab,
            help: if self.detail { BACK } else { HELP },
        };
        let Some(mut area) = chrome.draw(f, &self.theme) else {
            return;
        };
        let theme = &self.theme;
        if let (Some(e), Some(_)) = (&self.err, &self.snap) {
            // the title has the host, so the reason and the path are all it needs
            let msg = format!(
                "Reading failed, so this is from earlier: {} ({}).",
                e.why, e.path
            );
            let p = Paragraph::new(Line::styled(msg, theme.bad())).wrap(Wrap { trim: true });
            let [banner, rest] = Layout::vertical([
                Constraint::Length(height(&p, area.width).min(2)),
                Constraint::Fill(1),
            ])
            .areas(area);
            f.render_widget(p, banner);
            area = rest;
        }
        let Some(s) = &self.snap else {
            let msg;
            let n = match &self.err {
                Some(e) => {
                    msg = format!(
                        "Can't read the app at {}: {} ({}). {}",
                        self.base,
                        e.why,
                        e.path,
                        e.hint()
                    );
                    Notice::Error(&msg)
                }
                None => Notice::Loading,
            };
            f.render_widget(n.paragraph(theme), area);
            return;
        };
        let c = Ctx {
            theme,
            s,
            now: s.app_now(now),
        };
        let rows = &mut self.rows[self.tab];
        let picked = rows.selected().filter(|_| self.detail);
        let page = match (self.tab, picked) {
            (0, Some(i)) if i < s.strikes.strikes.len() => {
                strike_detail(&c, f, area, &s.strikes.strikes[i]);
                None
            }
            (1, Some(i)) if i < s.areas.areas.len() => {
                area_detail(&c, f, area, &s.areas.areas[i]);
                None
            }
            (2, Some(i)) if i < s.receivers.receivers.len() => {
                receiver_detail(&c, f, area, &s.receivers.receivers[i]);
                None
            }
            (0, _) => Some(strikes(&c, f, area, rows)),
            (1, _) => Some(areas(&c, f, area, rows)),
            _ => Some(receivers(&c, f, area, rows)),
        };
        if let Some(page) = page {
            self.page = page;
        }
        self.ask();
    }
}

/// What every page draws from.
struct Ctx<'a> {
    theme: &'a Theme,
    s: &'a Snap,
    /// by the app's clock
    now: i64,
}

/// A pill with a word of its own, for states the shared `Pill` has no word for.
fn tag(theme: &Theme, word: &str, c: Color) -> Span<'static> {
    Span::styled(format!(" {word} "), theme.pill(c))
}

/// How far a strike is checked, as its page on the web explains it.
fn check(s: &StrikeRow) -> (&'static str, Color) {
    if s.ambiguous {
        ("AMBIGUOUS", Color::Yellow)
    } else if s.unchecked {
        ("UNCHECKED", Color::Yellow)
    } else if s.rms_us.is_none() {
        ("EXACT", Color::Unknown)
    } else {
        ("CHECKED", Color::Green)
    }
}

fn area_state(a: &AreaRow) -> (&'static str, Color) {
    match a.state.as_str() {
        "dry" => ("DRY LIGHTNING", Color::Red),
        "lightning" => ("LIGHTNING", Color::Yellow),
        "quiet" => ("QUIET", Color::Green),
        _ => ("WAITING", Color::Unknown),
    }
}

/// A receiver's pill from the color the map page shows it in.
fn receiver_pill(r: &ReceiverRow) -> Pill {
    match r.color.as_str() {
        "green" => Pill::Ok,
        "yellow" => Pill::Partial,
        "red" => Pill::Down,
        _ => Pill::Stale,
    }
}

fn severity(theme: &Theme, a: &OpenAlert) -> Span<'static> {
    let c = if a.severity == "critical" {
        Color::Red
    } else {
        Color::Yellow
    };
    tag(theme, &a.severity.to_uppercase(), c)
}

/// The map's age classes: the last 15 minutes stand out and anything past the hour is
/// dimmed.
fn age_style(theme: &Theme, age_ms: i64) -> Style {
    if age_ms < 15 * 60_000 {
        Style::new().add_modifier(Modifier::BOLD)
    } else if age_ms < 3_600_000 {
        Style::new()
    } else {
        theme.mute()
    }
}

/// "12 s": how long before `now` (ms) something at `t_us` was.
fn ago(now: i64, t_us: i64) -> String {
    human_duration(now - t_us.div_euclid(1000))
}

/// "1.2 x 0.6 km": the 95% ellipse's semi-axes.
fn ellipse(s: &StrikeRow) -> String {
    format!("{:.1} x {:.1} km", s.major_km, s.minor_km)
}

fn strikes_n(n: usize) -> String {
    format!("{n} strike{}", if n == 1 { "" } else { "s" })
}

/// "14:02 UTC"
fn hhmm(ms: i64) -> String {
    short_time(ms)[11..].to_string()
}

/// To a tenth, without a "-0.0" for a tiny negative.
fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0 + 0.0
}

/// "+1.2 us", or "-" when there's nothing to say yet.
fn signed(v: Option<f64>, unit: &str) -> String {
    v.map_or("-".into(), |v| format!("{:+.1} {unit}", tenth(v)))
}

fn rain_short(r: &Rain) -> String {
    match (r.verdict.as_str(), r.mm) {
        (v @ ("dry" | "wet"), Some(mm)) => format!("{v}, {mm:.1} mm"),
        _ => "no data".into(),
    }
}

fn table<'a>(
    theme: &Theme,
    header: &[&'a str],
    widths: &[Constraint],
    rows: impl IntoIterator<Item = Row<'a>>,
) -> Table<'a> {
    Table::new(rows, widths.to_vec())
        .header(Row::new(header.to_vec()).style(theme.heading()))
        .row_highlight_style(theme.selected())
}

/// Rows a paragraph takes at `width` columns once wrapped, found by drawing it.
fn height(p: &Paragraph, width: u16) -> u16 {
    let area = Rect::new(0, 0, width.max(1), 200);
    let mut b = Buffer::empty(area);
    p.render(area, &mut b);
    (0..area.height)
        .rev()
        .find(|&y| (0..area.width).any(|x| b[(x, y)] != Spot::EMPTY))
        .map_or(0, |y| y + 1)
}

/// Text over a page's table, wrapped to fit, and the area left for the table.
fn above(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> Rect {
    // trimming would take the space off the front of a pill
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    let [head, rest] = Layout::vertical([
        Constraint::Length(height(&p, area.width) + 1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(p, head);
    rest
}

/// Rows a table drawn in `area` shows, under its header.
fn shown(area: Rect) -> usize {
    area.height.saturating_sub(1).into()
}

fn summary(c: &Ctx) -> String {
    let s = &c.s.strikes;
    let period = match s.hours {
        1 => "hour".to_string(),
        h => format!("{h} hours"),
    };
    let Some(newest) = s.strikes.first() else {
        return format!("No strikes in the last {period}.");
    };
    let shown = match s.strikes.len() {
        n if n < s.total => format!(" (newest {n} listed)"),
        _ => String::new(),
    };
    format!(
        "{} in the last {period}, the latest {} ago{shown}.",
        strikes_n(s.total),
        ago(c.now, newest.t_us)
    )
}

/// Why there are no strikes, or when there will be.
fn why_none(c: &Ctx) -> String {
    let solver = &c.s.strikes.solver;
    let heard = c.s.receivers.receivers.len();
    if !solver.enabled {
        "This site doesn't run the solver; strikes come from the site that does, through the core."
            .into()
    } else if heard < solver.min_sites {
        format!(
            "The solver needs {} receivers to hear a strike (min_sites) and {heard} reported this week.",
            solver.min_sites
        )
    } else {
        "Strikes show here once the solver has located them, settle_s after they happen.".into()
    }
}

/// The open alerts, critical first, for the top of the Strikes page.
fn alert_lines(c: &Ctx) -> Vec<Line<'static>> {
    let mut open: Vec<&OpenAlert> =
        c.s.areas
            .areas
            .iter()
            .filter_map(|a| a.alert.as_ref())
            .collect();
    open.sort_by_key(|a| (a.severity != "critical", a.opened_ms));
    let mut out: Vec<Line<'static>> = open
        .iter()
        .take(3)
        .map(|a| {
            let since = format!(
                " since {}{}",
                hhmm(a.opened_ms),
                if a.acked { ", acknowledged" } else { "" }
            );
            Line::from(vec![
                severity(c.theme, a),
                Span::raw(" "),
                Span::raw(a.title.clone()),
                Span::styled(since, c.theme.mute()),
            ])
        })
        .collect();
    if open.len() > 3 {
        out.push(Line::styled(
            format!("and {} more on the Areas page", open.len() - 3),
            c.theme.mute(),
        ));
    }
    out
}

/// The strikes list; returns how many rows its table shows.
fn strikes(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let s = &c.s.strikes;
    let mut lines = vec![Line::from(summary(c))];
    lines.extend(alert_lines(c));
    let area = above(f, area, lines);
    if s.strikes.is_empty() {
        f.render_widget(Notice::Empty(&why_none(c)).paragraph(c.theme), area);
        return shown(area);
    }
    // whether a strike is outside the ring is on its own page: at 80 columns the note
    // doesn't fit next to an area's name
    let items = s.strikes.iter().map(|x| {
        let (word, color) = check(x);
        let age = c.now - x.t_us.div_euclid(1000);
        Row::new([
            Cell::from(tag(c.theme, word, color)),
            Cell::from(Span::styled(human_duration(age), age_style(c.theme, age))),
            Cell::from(format!("{:.3}, {:.3}", x.lat, x.lon)),
            Cell::from(ellipse(x)),
            Cell::from(x.site_count.to_string()),
            Cell::from(x.areas.join(", ")),
        ])
    });
    let widths = [
        Constraint::Length(11),
        Constraint::Length(6),
        // "-35.380, -120.550"
        Constraint::Length(17),
        // "10.0 x 10.0 km"
        Constraint::Length(14),
        Constraint::Length(5),
        Constraint::Fill(1),
    ];
    let header = ["Check", "Age", "Position", "95% ellipse", "Sites", "Areas"];
    f.render_stateful_widget(table(c.theme, &header, &widths, items), area, rows);
    shown(area)
}

fn fit_text(s: &StrikeRow) -> String {
    let bearings = match s.bearings_in_fit {
        0 => String::new(),
        1 => " and a bearing".into(),
        n => format!(" and {n} bearings"),
    };
    match s.rms_us {
        Some(r) => format!(
            "{} receivers{bearings}: {r:.1} us RMS in arrival time.",
            s.site_count
        ),
        None => "Three receivers and no bearing in the fit: exact, so nothing checks it.".into(),
    }
}

/// A strike's details: what its page on the web says, then its arrivals once read.
fn strike_detail(c: &Ctx, f: &mut Frame, area: Rect, s: &StrikeRow) {
    let (word, color) = check(s);
    let mut lines = vec![
        Line::from(vec![
            tag(c.theme, word, color),
            Span::raw(" "),
            Span::styled(
                format!("Strike at {}, {} ago", s.time, ago(c.now, s.t_us)),
                c.theme.heading(),
            ),
        ]),
        Line::from(""),
        Line::from(format!(
            "{:.5}, {:.5}: inside an ellipse {:.2} by {:.2} km (semi-axes), its long axis {:.0} degrees from north, with 95% confidence.",
            s.lat, s.lon, s.major_km, s.minor_km, s.az_deg
        )),
        Line::from(fit_text(s)),
    ];
    let notes = [
        (
            s.ambiguous,
            "Ambiguous: another position fits these measurements as well, so it raises no alert.",
        ),
        (
            s.unchecked,
            "Unchecked: these three arrival times only fit once their bearings were left out, so it raises no alert.",
        ),
        (
            c.s.outside(s),
            "Outside the ring of receivers, where accuracy falls off faster than the ellipse shows.",
        ),
    ];
    for (_, note) in notes.iter().filter(|n| n.0) {
        lines.push(Line::from(*note));
    }
    if !s.areas.is_empty() {
        lines.push(Line::from(format!("In {}.", s.areas.join(", "))));
    }
    let list = above(f, area, lines);
    match &c.s.arrivals {
        Some(a) if a.id == s.id => f.render_widget(arrivals(c, s, &a.sites), list),
        _ => f.render_widget(
            Line::styled("Reading its arrivals...", c.theme.mute()),
            list,
        ),
    }
}

fn arrivals<'a>(c: &Ctx, s: &StrikeRow, sites: &'a [ArrivalRow]) -> Table<'a> {
    let rows = sites.iter().map(|a| {
        let bearing = a.bearing_deg.map_or("-".into(), |b| {
            let out = if a.bearing_left_out {
                " (left out)"
            } else {
                ""
            };
            format!("{b:.1} deg{out}")
        });
        Row::new([
            Cell::from(a.label.as_str()),
            Cell::from(format!("{} us", a.t_us - s.t_us)),
            Cell::from(signed(Some(a.resid_us), "us")),
            Cell::from(a.impulse.map_or("-".into(), |i| format!("{i:.2}"))),
            Cell::from(bearing),
            Cell::from(signed(a.bearing_resid_deg, "deg")),
        ])
    });
    let widths = [
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(20),
        Constraint::Fill(1),
    ];
    let header = [
        "Site",
        "After",
        "Residual",
        "Impulse",
        "Bearing",
        "Bearing resid",
    ];
    table(c.theme, &header, &widths, rows)
}

/// The watch areas; returns how many rows their table shows.
fn areas(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let a = &c.s.areas;
    if a.areas.is_empty() {
        let hint = "No watch areas are set. Add [[watch]] areas to the config for an alert when lightning lands in one.";
        f.render_widget(Notice::Empty(hint).paragraph(c.theme), area);
        return 0;
    }
    let area = above(
        f,
        area,
        vec![Line::styled(
            format!(
                "Strikes of the last {} min; an area's alert stays open while it has any.",
                a.window_min
            ),
            c.theme.mute(),
        )],
    );
    let items = a.areas.iter().map(|x| {
        let (word, color) = area_state(x);
        let strikes = match (x.state.as_str(), x.capped) {
            ("waiting", _) => "-".to_string(),
            (_, true) => format!("{}+", x.strikes),
            _ => x.strikes.to_string(),
        };
        let alert = match &x.alert {
            Some(o) => format!(
                "since {}{}",
                hhmm(o.opened_ms),
                if o.acked { ", acknowledged" } else { "" }
            ),
            None if x.state == "waiting" => format!(
                "no position for {}",
                x.site_label.as_deref().unwrap_or("its site")
            ),
            None => "none".into(),
        };
        Row::new([
            Cell::from(tag(c.theme, word, color)),
            Cell::from(x.name.clone()),
            Cell::from(strikes),
            Cell::from(rain_short(&x.rain)),
            Cell::from(alert),
        ])
    });
    let widths = [
        Constraint::Length(15),
        Constraint::Length(16),
        Constraint::Length(7),
        Constraint::Length(12),
        Constraint::Fill(1),
    ];
    let header = ["State", "Area", "Strikes", "Rain", "Alert"];
    f.render_stateful_widget(table(c.theme, &header, &widths, items), area, rows);
    shown(area)
}

fn area_detail(c: &Ctx, f: &mut Frame, area: Rect, a: &AreaRow) {
    let (word, color) = area_state(a);
    let window = c.s.areas.window_min;
    let shape = match (&a.site_label, a.center, a.radius_km) {
        (Some(site), _, Some(r)) => format!("Within {r} km of {site}."),
        (None, Some(p), Some(r)) => format!("Within {r} km of {:.4}, {:.4}.", p[0], p[1]),
        _ => format!("A polygon with {} corners.", a.corners),
    };
    let count = match a.latest_us {
        Some(t) => format!(
            "{}{} in the last {window} min, the latest {} ago.",
            if a.capped { "More than " } else { "" },
            strikes_n(a.strikes),
            ago(c.now, t)
        ),
        None if a.state == "waiting" => {
            "Its site's position isn't known yet (no receiver report or node poll), so it raises nothing."
                .into()
        }
        None => format!("No strikes in the last {window} min."),
    };
    let mut lines = vec![
        Line::from(vec![
            tag(c.theme, word, color),
            Span::raw(" "),
            Span::styled(a.name.clone(), c.theme.heading()),
        ]),
        Line::from(""),
        Line::from(shape),
        Line::from(count),
        Line::from(format!("Rain: {}", a.rain.text)),
        Line::from(""),
    ];
    match &a.alert {
        Some(o) => {
            lines.push(Line::from(vec![
                severity(c.theme, o),
                Span::raw(format!(
                    " {}, open since {}{}",
                    o.title,
                    short_time(o.opened_ms),
                    if o.acked {
                        ", acknowledged"
                    } else {
                        ", not acknowledged"
                    }
                )),
            ]));
            lines.push(Line::from(o.text.clone()));
        }
        None => lines.push(Line::styled("No alert is open for it.", c.theme.mute())),
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// The receivers; returns how many rows their table shows.
fn receivers(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let list = &c.s.receivers.receivers;
    if list.is_empty() {
        let hint = "No receiver has reported in the last week. Each site with a VLF receiver runs this app with a [receiver] section; its reports reach here through the core.";
        f.render_widget(Notice::Empty(hint).paragraph(c.theme), area);
        return 0;
    }
    let fine = list.iter().filter(|r| receiver_pill(r) == Pill::Ok).count();
    let area = above(
        f,
        area,
        vec![Line::styled(
            format!(
                "{fine} of {} without a warning. Timing and bearings: median residuals this week.",
                list.len()
            ),
            c.theme.mute(),
        )],
    );
    let items = list.iter().map(|r| {
        (
            Some(receiver_pill(r)),
            vec![
                Cell::from(r.label.clone()),
                Cell::from(r.sferics_last_hour.to_string()),
                Cell::from(signed(r.median_residual_us, "us")),
                Cell::from(signed(r.median_bearing_residual_deg, "deg")),
                Cell::from(r.state.clone()),
            ],
        )
    });
    let widths = [
        Constraint::Length(12),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Fill(1),
    ];
    let header = ["Site", "Sferics/h", "Timing", "Bearings", "Why"];
    f.render_stateful_widget(pill_table(c.theme, &header, &widths, items), area, rows);
    shown(area)
}

fn receiver_detail(c: &Ctx, f: &mut Frame, area: Rect, r: &ReceiverRow) {
    let name = if r.label == r.site {
        r.site.clone()
    } else {
        format!("{} ({})", r.label, r.site)
    };
    let gps = match r.gps.as_str() {
        "unchecked" => "not checked (no gpsd)",
        g => g,
    };
    let dropped = match r.dropped_last_hour {
        0 => String::new(),
        n => format!(" ({n} dropped)"),
    };
    let last = r.last_sferic_us.map_or("none in 2 days".into(), |t| {
        format!("the last {} ago", ago(c.now, t))
    });
    let median = |m: Option<f64>, unit: &str| {
        m.map(|m| format!("; median residual {:+.1} {unit}", tenth(m)))
            .unwrap_or_default()
    };
    let bearings = match r.bearing_sigma_deg {
        None => "Bearings: none this week.".to_string(),
        Some(s) => format!(
            "Bearings: {s} deg each (1 sigma){}{}.",
            median(r.median_bearing_residual_deg, "deg"),
            if r.bearings_left_out {
                ", so they're left out of fits"
            } else {
                ""
            }
        ),
    };
    let mut lines = vec![
        Line::from(vec![
            tui::pill(c.theme, receiver_pill(r)),
            Span::raw(" "),
            Span::styled(name, c.theme.heading()),
        ]),
        Line::from(""),
        Line::from(format!(
            "State: {}. GPS: {gps}. Last report {} ago.",
            r.state,
            human_duration(c.now - r.last_report_ms)
        )),
        Line::from(format!("Antenna at {:.4}, {:.4}.", r.lat, r.lon)),
        Line::from(format!(
            "{} sferics in the last hour{dropped}; {last}. In {} this week.",
            r.sferics_last_hour,
            strikes_n(r.strikes_7d)
        )),
        Line::from(format!(
            "Timing: {:.0} us per arrival (1 sigma){}.",
            r.sigma_us,
            median(r.median_residual_us, "us")
        )),
        Line::from(bearings),
    ];
    if let Some(s) = r.clock_s {
        lines.push(Line::from(format!(
            "Its newest arrival time is {s:+.1} s from its computer's clock."
        )));
    }
    if let Some(e) = &r.error {
        lines.push(Line::styled(format!("Error: {e}"), c.theme.bad()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use axum::response::IntoResponse;
    use ridgewarden_client::tui::MIN_SIZE;
    use ridgewarden_client::tui::ratatui::Terminal;
    use ridgewarden_client::tui::ratatui::backend::TestBackend;
    use ridgewarden_client::tui::ratatui::crossterm::event::KeyModifiers;
    use ridgewarden_client::tui::ratatui::style::Color as Ansi;
    use ridgewarden_client::util::esc;
    use serde_json::{Value, json};

    use crate::{testing, web};

    fn top(snap: Option<Snap>) -> Top {
        Top {
            base: "http://127.0.0.1:8796".into(),
            theme: Theme { color: false },
            refresh: Refresh::spawn(Duration::from_secs(3600), async || {
                Err(failed("not read in this test", Kind::Other))
            })
            .unwrap(),
            asks: Arc::default(),
            snap,
            err: None,
            tab: 0,
            rows: Default::default(),
            detail: false,
            page: 10,
        }
    }

    fn loader(base: String, asks: Arc<Asks>) -> Loader {
        Loader {
            // Windows takes a couple of seconds to give up on a closed port
            fetch: Fetch::new(Duration::from_secs(20)).unwrap(),
            base,
            asks,
            receivers: None,
            arrivals: None,
        }
    }

    fn failed(why: &str, kind: Kind) -> Failed {
        Failed {
            path: "/api/strikes?limit=50".into(),
            why: why.into(),
            kind,
        }
    }

    fn press(t: &mut Top, code: KeyCode) {
        assert!(t.key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    /// The text on a `w` by `h` terminal after one draw.
    fn screen(top: &mut Top, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| top.draw(f)).unwrap();
        let b = t.backend().buffer();
        let rows = (0..h).map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect::<String>());
        rows.collect::<Vec<_>>().join("\n")
    }

    /// The screen's words with each run of spaces and line breaks as one space, for text
    /// that wraps.
    fn flat(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn row<'a>(s: &'a str, start: &str) -> &'a str {
        s.lines()
            .find(|l| l.starts_with(start))
            .unwrap_or_else(|| panic!("no row starting {start:?} in\n{s}"))
            .trim_end()
    }

    fn json_row<T: serde::de::DeserializeOwned>(v: Value) -> T {
        serde_json::from_value(v).unwrap()
    }

    fn strike_value(id: &str, t_us: i64) -> Value {
        json!({"id": id, "time": web::strike_time(t_us), "t_us": t_us, "lat": 35.3, "lon": -120.6,
            "major_km": 1.0, "minor_km": 0.5, "az_deg": 0.0, "rms_us": 1.0, "ambiguous": false,
            "unchecked": false, "areas": [], "site_count": 4, "bearings_in_fit": 0})
    }

    fn area_value(name: &str, state: &str, alert: Option<&str>) -> Value {
        let alert = alert.map(|severity| {
            json!({"severity": severity, "title": format!("Lightning in {name}"), "text": "",
                "opened_ms": 0, "acked": false})
        });
        json!({"name": name, "site_label": null, "center": null, "radius_km": null,
            "corners": 3, "strikes": 0, "capped": false, "latest_us": null, "state": state,
            "rain": {"verdict": "missing", "mm": null, "text": ""}, "alert": alert})
    }

    /// A read whose strikes have these ids, newest first, a minute apart.
    fn snap_with(ids: &[&str], total: usize) -> Snap {
        let now = now_ms();
        let strikes: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| strike_value(id, (now - 60_000 * (i as i64 + 1)) * 1000))
            .collect();
        Snap {
            at: now,
            strikes: json_row(json!({"generated": now, "hours": 3, "total": total,
                "solver": {"enabled": true, "min_sites": 4}, "strikes": strikes})),
            areas: json_row(json!({"window_min": 30, "areas": []})),
            receivers: json_row(json!({"receivers": []})),
            ring: vec![],
            arrivals: None,
        }
    }

    /// A stand-in for the app: it answers each path in `answers` with its JSON and anything
    /// else with a 404, and keeps each path and query it was asked for.
    async fn stand_in(answers: HashMap<&'static str, Value>) -> (String, Arc<Mutex<Vec<String>>>) {
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (log, answers) = (asked.clone(), Arc::new(answers));
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let (log, answers) = (log.clone(), answers.clone());
            async move {
                log.lock().unwrap().push(uri.to_string());
                match answers.get(uri.path()) {
                    Some(v) => axum::Json(v.clone()).into_response(),
                    None => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        (base, asked)
    }

    #[tokio::test]
    async fn top_shows_what_the_app_serves() {
        let dir = tempfile::tempdir().unwrap();
        let base = testing::serve(testing::storm(dir.path())).await;
        let mut t = top(None);
        let mut l = loader(base, t.asks.clone());
        t.take(l.load().await);
        let s = t.snap.as_ref().unwrap();
        let checks: Vec<&str> = s.strikes.strikes.iter().map(|x| check(x).0).collect();
        assert_eq!(
            checks,
            ["CHECKED", "EXACT", "UNCHECKED", "AMBIGUOUS"],
            "newest first"
        );
        let outside: Vec<bool> = s.strikes.strikes.iter().map(|x| s.outside(x)).collect();
        assert_eq!(outside, [false, false, false, true]);
        let states: Vec<&str> = s.areas.areas.iter().map(|a| area_state(a).0).collect();
        assert_eq!(states, ["DRY LIGHTNING", "LIGHTNING", "WAITING"]);
        let pills: Vec<(&str, Pill)> = s
            .receivers
            .receivers
            .iter()
            .map(|r| (r.label.as_str(), receiver_pill(r)))
            .collect();
        assert_eq!(
            pills,
            [
                ("tst-east", Pill::Ok),
                ("North ridge", Pill::Ok),
                ("tst-south", Pill::Ok),
                ("tst-west", Pill::Down)
            ]
        );
        assert!(s.arrivals.is_none(), "no strike open, so no arrivals read");

        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains("4 strikes in the last 3 hours, the latest 2 min ago."),
            "{s}"
        );
        assert!(
            row(&s, " CRITICAL ").contains("Dry lightning in Backcountry since"),
            "critical first: {s}"
        );
        assert!(row(&s, " WARNING ").contains("Lightning in Near the hub since"));
        let first = row(&s, " CHECKED ");
        assert!(first.contains("35.380, -120.550  1.2 x 0.6 km   4"), "{s}");
        assert!(first.ends_with("Backcountry"), "{s}");
        assert!(row(&s, " EXACT ").ends_with("Near the hub"), "{s}");
        assert!(
            row(&s, " AMBIGUOUS ").ends_with("35.000, -121.200  1.2 x 0.6 km   3"),
            "in no area: {s}"
        );

        press(&mut t, KeyCode::Enter);
        assert!(screen(&mut t, 80, 24).contains("Reading its arrivals..."));
        assert_eq!(t.asks.strike.lock().unwrap().as_deref(), Some("0a"));
        t.take(l.load().await);
        let d = screen(&mut t, 80, 24);
        assert!(d.contains(" CHECKED  Strike at "), "{d}");
        let north = row(&d, "North ridge");
        assert!(north.contains("0 us") && north.contains("+0.0 us"), "{d}");
        assert!(row(&d, "tst-east").contains("41.0 deg (left out)"), "{d}");
        let d = flat(&d);
        assert!(
            d.contains("4 receivers and 3 bearings: 2.1 us RMS in arrival time."),
            "{d}"
        );
        assert!(d.contains("In Backcountry.") && !d.contains("Outside the ring"));
        press(&mut t, KeyCode::Down);
        let d = flat(&screen(&mut t, 80, 24));
        assert!(
            d.contains("EXACT Strike at") && d.contains("nothing checks it"),
            "Down moves to the next strike: {d}"
        );
        press(&mut t, KeyCode::End);
        let d = flat(&screen(&mut t, 80, 24));
        assert!(
            d.contains("AMBIGUOUS Strike at")
                && d.contains("another position fits these measurements as well")
                && d.contains("Outside the ring of receivers"),
            "{d}"
        );
        press(&mut t, KeyCode::Esc);
        assert!(screen(&mut t, 80, 24).contains("95% ellipse"));

        press(&mut t, KeyCode::Char('2'));
        let a = screen(&mut t, 80, 24);
        let back = row(&a, " DRY LIGHTNING ");
        assert!(
            back.contains("Backcountry") && back.contains("dry, 0.3 mm") && back.contains("since"),
            "{a}"
        );
        assert!(
            row(&a, " WAITING ").ends_with("no position for tst-new"),
            "{a}"
        );
        press(&mut t, KeyCode::Enter);
        let a = flat(&screen(&mut t, 80, 24));
        assert!(
            a.contains("Rain: Dry: 0.3 mm of rain in the last 24 h at CW0001"),
            "{a}"
        );
        assert!(
            a.contains("Dry lightning in Backcountry, open since"),
            "{a}"
        );

        press(&mut t, KeyCode::Tab);
        let r = screen(&mut t, 80, 24);
        assert!(r.contains("3 of 4 without a warning."), "{r}");
        let west = row(&r, " DOWN ");
        assert!(
            west.contains("tst-west") && west.contains("GPS not locked"),
            "{r}"
        );

        for tab in 0..TABS.len() {
            t.tab = tab;
            for detail in [false, true] {
                t.detail = detail;
                let s = screen(&mut t, MIN_SIZE.0, MIN_SIZE.1);
                assert!(!s.contains("Make the terminal"), "{s}");
            }
        }
        assert!(screen(&mut t, MIN_SIZE.0 - 1, MIN_SIZE.1).contains("Make the terminal at least"));
    }

    #[tokio::test]
    async fn receivers_are_read_every_30_s_and_a_strike_once() {
        let strikes = json!({"generated": 0, "hours": 3, "total": 1,
            "solver": {"enabled": true, "min_sites": 4}, "strikes": [strike_value("0a", 0)]});
        let arrival = json!({"label": "tst-a", "t_us": 1, "resid_us": 0.0, "impulse": null,
            "bearing_deg": null, "bearing_resid_deg": null, "bearing_left_out": false});
        let (base, asked) = stand_in(HashMap::from([
            ("/api/strikes", strikes),
            ("/api/areas", json!({"window_min": 30, "areas": []})),
            ("/api/receivers", json!({"receivers": []})),
            ("/api/strike", json!({"id": "0a", "sites": [arrival]})),
        ]))
        .await;
        let reads = |path: &str| {
            let asked = asked.lock().unwrap();
            asked
                .iter()
                .filter(|u| u.split('?').next() == Some(path))
                .count()
        };
        let asks = Arc::new(Asks::default());
        let mut l = loader(base, asks.clone());
        l.load().await.unwrap();
        l.load().await.unwrap();
        assert_eq!((reads("/api/strikes"), reads("/api/receivers")), (2, 1));
        l.receivers.as_mut().unwrap().0 -= SLOW_MS;
        l.load().await.unwrap();
        assert_eq!(reads("/api/receivers"), 2, "again once it's 30 s old");
        asks.receivers.store(true, Ordering::Relaxed);
        l.load().await.unwrap();
        assert_eq!(reads("/api/receivers"), 3, "and right away after r");

        assert_eq!(reads("/api/strike"), 0, "no strike's details open");
        *asks.strike.lock().unwrap() = Some("0a".into());
        asks.rows.store(32, Ordering::Relaxed);
        let s = l.load().await.unwrap();
        assert_eq!(s.arrivals.map(|a| a.sites.len()), Some(1));
        l.load().await.unwrap();
        assert_eq!(reads("/api/strike"), 1, "a strike's arrivals don't change");
        let strikes_read: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|u| u.starts_with("/api/strikes?"))
            .cloned()
            .collect();
        assert_eq!(
            strikes_read.last().map(String::as_str),
            Some("/api/strikes?limit=32"),
            "as many strikes as the screen asked for"
        );
    }

    #[test]
    fn r_and_the_screen_tell_the_loader_what_to_read() {
        let ids: Vec<String> = (0..40).map(|i| format!("{i:02x}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut snap = snap_with(&ids, 40);
        for (i, s) in snap.strikes.strikes.iter_mut().enumerate() {
            s.lat = 35.0 + i as f64 / 1000.0;
        }
        snap.areas = json_row(json!({"window_min": 30, "areas": [
            area_value("Backcountry", "dry", Some("critical")),
            area_value("Near the hub", "lightning", Some("warning")),
        ]}));
        let mut t = top(Some(snap));
        let first = screen(&mut t, 80, 24);
        // the summary, two alerts, a blank line and the header leave 16 of the 21 rows
        assert_eq!(t.page, 16, "{first}");
        assert_eq!(t.asks.rows.load(Ordering::Relaxed), 32, "two pages' worth");
        assert!(
            first.contains("35.015,") && !first.contains("35.016,"),
            "{first}"
        );
        press(&mut t, KeyCode::PageDown);
        press(&mut t, KeyCode::PageDown);
        let s = screen(&mut t, 80, 24);
        let top_row = s.lines().skip_while(|l| !l.starts_with("Check")).nth(1);
        assert!(
            top_row.is_some_and(|l| l.contains("35.016,")),
            "the second page starts where the first ended: {s}"
        );
        assert_eq!(
            t.asks.rows.load(Ordering::Relaxed),
            31 + 32,
            "two pages past the selection"
        );
        press(&mut t, KeyCode::Char('r'));
        assert!(t.asks.receivers.load(Ordering::Relaxed));
        press(&mut t, KeyCode::Enter);
        assert_eq!(t.asks.strike.lock().unwrap().as_deref(), Some("1f"));
        press(&mut t, KeyCode::Esc);
        assert_eq!(*t.asks.strike.lock().unwrap(), None);
    }

    #[test]
    fn the_selection_stays_on_its_strike_as_new_ones_arrive() {
        let mut t = top(Some(snap_with(&["c", "b", "a"], 3)));
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Down);
        press(&mut t, KeyCode::Enter);
        assert_eq!((t.rows[0].selected(), t.detail), (Some(1), true));
        t.take(Ok(snap_with(&["e", "d", "c", "b", "a"], 5)));
        assert_eq!(
            (t.rows[0].selected(), t.detail),
            (Some(3), true),
            "still on b"
        );
        t.take(Ok(snap_with(&["f", "e"], 2)));
        assert_eq!(
            (t.rows[0].selected(), t.detail),
            (None, false),
            "b is gone, and its details with it"
        );
    }

    #[test]
    fn keys_switch_pages_and_open_details() {
        let mut t = top(Some(snap_with(&["b"], 54_321)));
        let s = screen(&mut t, 80, 24);
        assert!(
            s.contains("54321 strikes in the last 3 hours, the latest 60 s ago (newest 1 listed)."),
            "a big storm's count fits 80 columns: {s}"
        );
        t.take(Ok(snap_with(&["b", "a"], 2)));
        let s = screen(&mut t, 80, 24);
        assert_eq!(
            row(&s, "2 strikes"),
            "2 strikes in the last 3 hours, the latest 60 s ago."
        );
        press(&mut t, KeyCode::Enter);
        assert_eq!(
            (t.rows[0].selected(), t.detail),
            (Some(0), true),
            "Enter picks the first strike"
        );
        assert!(screen(&mut t, 80, 24).contains(BACK));
        press(&mut t, KeyCode::Enter);
        assert!(!t.detail, "Enter again goes back");
        press(&mut t, KeyCode::Enter);
        press(&mut t, KeyCode::Right);
        assert_eq!(
            (t.tab, t.detail),
            (1, false),
            "a new page starts on its list"
        );
        press(&mut t, KeyCode::Enter);
        assert!(!t.detail, "no areas, nothing to open");
        let s = flat(&screen(&mut t, 80, 24));
        assert!(s.contains("No watch areas are set."), "{s}");
        press(&mut t, KeyCode::Char('3'));
        assert_eq!(t.tab, 2);
        let s = flat(&screen(&mut t, 80, 24));
        assert!(
            s.contains("No receiver has reported in the last week."),
            "{s}"
        );
        press(&mut t, KeyCode::BackTab);
        press(&mut t, KeyCode::BackTab);
        assert_eq!(t.tab, 0);
        assert!(screen(&mut t, 80, 24).contains(HELP));
    }

    #[test]
    fn no_strikes_says_why() {
        let mut t = top(None);
        let mut snap = snap_with(&[], 0);
        snap.strikes.solver.enabled = false;
        t.take(Ok(snap));
        let s = flat(&screen(&mut t, 80, 24));
        assert!(s.contains("No strikes in the last 3 hours."), "{s}");
        assert!(s.contains("This site doesn't run the solver"), "{s}");
        let mut snap = snap_with(&[], 0);
        snap.strikes.hours = 1;
        t.take(Ok(snap));
        let s = flat(&screen(&mut t, 80, 24));
        assert!(
            s.contains("No strikes in the last hour.")
                && s.contains("needs 4 receivers to hear a strike (min_sites) and 0 reported"),
            "{s}"
        );
    }

    #[tokio::test]
    async fn an_app_that_isnt_there_is_said_plainly() {
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let mut t = top(None);
        t.base.clone_from(&closed);
        let e = loader(closed.clone(), t.asks.clone())
            .load()
            .await
            .err()
            .unwrap();
        assert_eq!(e, failed("could not connect", Kind::Unreachable));
        assert!(screen(&mut t, 80, 24).contains("Loading..."));
        t.take(Err(e.clone()));
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("app not answering"), "{s}");
        let s = flat(&s);
        assert!(
            s.contains(&format!(
                "ERROR Can't read the app at {closed}: could not connect (/api/strikes?limit=50)."
            )) && s.contains("Is ridgewarden-lightning running there?")
                && s.contains("sudo systemctl start ridgewarden-lightning"),
            "{s}"
        );
        // after a good read, a failure keeps the data on screen and says why over it
        t.take(Ok(snap_with(&["b", "a"], 2)));
        t.take(Err(e));
        let s = screen(&mut t, 80, 24);
        assert!(s.contains("app not answering, updated"), "{s}");
        assert!(s.contains("95% ellipse"), "the last good read stays: {s}");
        assert!(
            flat(&s).contains(
                "Reading failed, so this is from earlier: could not connect (/api/strikes?limit=50)."
            ),
            "the reason isn't cut off: {s}"
        );
        // an app stopped while top runs leaves pooled connections that just close
        let hangs_up = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            std::thread::spawn(move || l.incoming().for_each(drop));
            url
        };
        let e = loader(hangs_up, Arc::default()).load().await.err().unwrap();
        assert_eq!(e, failed("request failed", Kind::Unreachable));
    }

    #[tokio::test]
    async fn an_older_app_is_said_to_answer_in_another_format() {
        // one without /api/strikes, and one whose strikes lack this version's fields
        let (missing, _) = stand_in(HashMap::new()).await;
        let (old, _) = stand_in(HashMap::from([("/api/strikes", json!({"strikes": []}))])).await;
        for (base, why) in [(missing, "HTTP 404"), (old, "unexpected JSON")] {
            let mut t = top(None);
            t.base.clone_from(&base);
            let e = loader(base.clone(), t.asks.clone())
                .load()
                .await
                .err()
                .unwrap();
            assert_eq!(e, failed(why, Kind::OtherFormat));
            t.take(Err(e));
            let s = screen(&mut t, 80, 24);
            assert!(s.contains("read failed"), "{s}");
            let s = flat(&s);
            assert!(
                s.contains(&format!(
                    "Can't read the app at {base}: {why} (/api/strikes?limit=50)."
                )) && s.contains("It answered in another version's format"),
                "{s}"
            );
            assert!(!s.contains("running there"), "{s}");
        }
    }

    #[test]
    fn long_text_wraps_and_wide_ellipses_fit_80_columns() {
        let mut snap = snap_with(&["0a"], 1);
        let x = &mut snap.strikes.strikes[0];
        (x.major_km, x.minor_km, x.lat, x.lon) = (10.0, 10.0, 36.5, -121.5);
        (x.ambiguous, x.unchecked) = (true, true);
        x.areas = vec!["Backcountry".into(), "Near the hub".into()];
        snap.ring = vec![
            LatLon::new(35.0, -120.0),
            LatLon::new(35.5, -120.5),
            LatLon::new(35.0, -121.0),
        ];
        let long = "The backcountry north and east of the hub, up to the ridge";
        snap.areas = json_row(json!({"window_min": 30,
            "areas": [area_value(long, "dry", Some("critical"))]}));
        let mut t = top(Some(snap));
        let s = screen(&mut t, 80, 24);
        assert!(row(&s, " AMBIGUOUS ").contains("10.0 x 10.0 km 4"), "{s}");
        assert!(
            flat(&s).contains(&format!("CRITICAL Lightning in {long} since 00:00 UTC")),
            "the alert wraps: {s}"
        );
        press(&mut t, KeyCode::Enter);
        let d = flat(&screen(&mut t, 80, 24));
        for note in [
            "Ambiguous: another position fits these measurements as well, so it raises no alert.",
            "Unchecked: these three arrival times only fit once their bearings were left out, so it raises no alert.",
            "Outside the ring of receivers, where accuracy falls off faster than the ellipse shows.",
            "In Backcountry, Near the hub.",
            "Reading its arrivals...",
        ] {
            assert!(d.contains(note), "{note:?} isn't all there: {d}");
        }
    }

    #[test]
    fn rows_get_the_words_and_colors_of_the_pages() {
        let s = |extra: Value| {
            let mut v = strike_value("0a", 0);
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            check(&json_row(v))
        };
        assert_eq!(s(json!({})), ("CHECKED", Color::Green));
        assert_eq!(s(json!({"rms_us": null})), ("EXACT", Color::Unknown));
        assert_eq!(s(json!({"unchecked": true})), ("UNCHECKED", Color::Yellow));
        assert_eq!(
            s(json!({"ambiguous": true, "unchecked": true, "rms_us": null})),
            ("AMBIGUOUS", Color::Yellow),
            "ambiguous says the most"
        );
        let area = |state: &str| area_state(&json_row(area_value("a", state, None)));
        assert_eq!(area("dry"), ("DRY LIGHTNING", Color::Red));
        assert_eq!(area("lightning"), ("LIGHTNING", Color::Yellow));
        assert_eq!(area("quiet"), ("QUIET", Color::Green));
        assert_eq!(area("waiting"), ("WAITING", Color::Unknown));
        let theme = Theme { color: true };
        let alert = |sev: &str| {
            let a: AreaRow = json_row(area_value("a", "dry", Some(sev)));
            severity(&theme, &a.alert.unwrap())
        };
        assert_eq!(alert("critical").style, theme.pill(Color::Red));
        assert_eq!(alert("warning").content, " WARNING ");
        assert_eq!(alert("warning").style, theme.pill(Color::Yellow));
        let min = 60_000;
        assert!(
            age_style(&theme, 14 * min)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(age_style(&theme, 15 * min), Style::new());
        assert_eq!(age_style(&theme, 61 * min), theme.mute());
    }

    #[test]
    fn numbers_read_plainly() {
        assert_eq!(signed(Some(-0.04), "us"), "+0.0 us");
        assert_eq!(signed(Some(-11.26), "deg"), "-11.3 deg");
        assert_eq!(signed(None, "us"), "-");
        let rain = |v: Value| rain_short(&json_row(v));
        assert_eq!(
            rain(json!({"verdict": "dry", "mm": 0.3, "text": ""})),
            "dry, 0.3 mm"
        );
        assert_eq!(
            rain(json!({"verdict": "wet", "mm": 6.35, "text": ""})),
            "wet, 6.3 mm"
        );
        assert_eq!(
            rain(json!({"verdict": "missing", "mm": null, "text": ""})),
            "no data"
        );
        assert_eq!(hhmm(1_795_000_000_123), "11:06 UTC");
        assert_eq!(strikes_n(1), "1 strike");
        assert_eq!(strikes_n(2), "2 strikes");
        let now = 1_795_000_000_000;
        assert_eq!(ago(now, (now - 89_500) * 1000), "89 s");
        assert_eq!(
            ago(now, (now - 90_000) * 1000 + 999),
            "1 min",
            "to the millisecond"
        );
        let x: StrikeRow = json_row(strike_value("0a", 0));
        assert_eq!(ellipse(&x), "1.0 x 0.5 km");
        assert_eq!(fit_text(&x), "4 receivers: 1.0 us RMS in arrival time.");
    }

    /// What the solver site's app answered at the end of a storm on the simulator, read
    /// two seconds ago, with the newest strike's arrivals.
    fn storm() -> Snap {
        let read = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        let receivers: Receivers = json_row(read(include_str!("../tests/fixtures/receivers.json")));
        Snap {
            at: now_ms() - 2000,
            strikes: json_row(read(include_str!("../tests/fixtures/strikes.json"))),
            areas: json_row(read(include_str!("../tests/fixtures/areas.json"))),
            ring: receivers.ring(),
            receivers,
            arrivals: Some(json_row(read(include_str!(
                "../tests/fixtures/strike.json"
            )))),
        }
    }

    /// With README_SHOTS=1 it also writes the Strikes page as
    /// <temp dir>/ridgewarden-lightning-top.html, which docs/images/top.png is a picture of.
    #[test]
    fn a_storm_on_the_simulator_fits_80x24() {
        let mut t = top(Some(storm()));
        let s = screen(&mut t, 80, 24);
        let head = row(&s, "376 strikes");
        assert!(
            head.starts_with("376 strikes in the last 3 hours, the latest ")
                && head.ends_with(" ago (newest 32 listed)."),
            "{s}"
        );
        assert!(
            row(&s, " CRITICAL ").ends_with("Dry lightning in Backcountry since 23:57 UTC"),
            "{s}"
        );
        assert!(
            row(&s, " WARNING ").ends_with("Lightning in Near the hub since 23:53 UTC"),
            "{s}"
        );
        assert!(
            row(&s, " CHECKED ").ends_with("35.521, -120.353  3.3 x 2.5 km   3"),
            "{s}"
        );
        assert!(
            row(&s, " UNCHECKED ").ends_with("35.486, -120.371  14.1 x 3.0 km  3     Backcountry"),
            "{s}"
        );
        press(&mut t, KeyCode::Enter);
        let d = screen(&mut t, 80, 24);
        assert!(row(&d, "tst-east").contains("179.1 deg"), "{d}");
        assert!(row(&d, "tst-south").ends_with('-'), "no loops there: {d}");
        let d = flat(&d);
        assert!(
            d.contains("3 receivers and 2 bearings:")
                && d.contains("Outside the ring of receivers"),
            "{d}"
        );
        press(&mut t, KeyCode::Esc);
        press(&mut t, KeyCode::Char('2'));
        let a = screen(&mut t, 80, 24);
        assert_eq!(
            row(&a, " DRY LIGHTNING "),
            " DRY LIGHTNING  Backcountry      115     dry, 0.3 mm  since 23:57 UTC"
        );
        assert_eq!(
            row(&a, " LIGHTNING "),
            " LIGHTNING      Near the hub     97      wet, 6.4 mm  since 23:53 UTC"
        );
        assert_eq!(
            row(&a, " QUIET "),
            " QUIET          Near North       0       no data      none"
        );
        press(&mut t, KeyCode::Char('3'));
        let r = screen(&mut t, 80, 24);
        assert!(r.contains("3 of 4 without a warning."), "{r}");
        let west = row(&r, " DOWN ");
        assert!(
            west.starts_with(" DOWN     tst-west     313") && west.ends_with("silent for 4 min"),
            "{r}"
        );
        for tab in 0..TABS.len() {
            t.tab = tab;
            for detail in [false, true] {
                t.detail = detail;
                assert!(!screen(&mut t, 80, 24).contains("Make the terminal"));
            }
        }
        if std::env::var_os("README_SHOTS").is_some_and(|v| v == "1") {
            readme_shot(t);
        }
    }

    fn readme_shot(mut t: Top) {
        (t.tab, t.detail, t.rows) = (0, false, Default::default());
        t.theme = Theme { color: true };
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| t.draw(f)).unwrap();
        let page = std::env::temp_dir().join("ridgewarden-lightning-top.html");
        std::fs::write(page, html(term.backend().buffer())).unwrap();
    }

    /// The terminal as a page of colored monospace cells, as a dark terminal shows them.
    fn html(b: &Buffer) -> String {
        let (bg, fg, dim) = ("#1d2125", "#d7dce1", "#8b949e");
        let rgb = |c: Ansi| match c {
            Ansi::Black => Some("#1d2125"),
            Ansi::Red => Some("#c9473d"),
            Ansi::Green => Some("#56b36a"),
            Ansi::Yellow => Some("#d8b14a"),
            Ansi::Gray => Some("#a8b0b9"),
            Ansi::White => Some("#f5f7f9"),
            _ => None,
        };
        let mut out = format!(
            "<!doctype html><meta charset=utf-8><title>ridgewarden-lightning top</title><style>\
             body{{margin:0;background:transparent}}pre{{display:inline-block;margin:0;\
             padding:14px 18px;border-radius:8px;background:{bg};color:{fg};\
             font:15px/1.4 'Cascadia Mono',Consolas,'DejaVu Sans Mono',monospace}}</style><pre>"
        );
        for y in 0..b.area.height {
            let mut x = 0;
            while x < b.area.width {
                let c = &b[(x, y)];
                let mut text = String::new();
                while x < b.area.width
                    && (b[(x, y)].fg, b[(x, y)].bg, b[(x, y)].modifier) == (c.fg, c.bg, c.modifier)
                {
                    text.push_str(b[(x, y)].symbol());
                    x += 1;
                }
                let (mut color, mut back) = (rgb(c.fg).unwrap_or(fg), rgb(c.bg));
                if c.modifier.contains(Modifier::REVERSED) {
                    (color, back) = (back.unwrap_or(bg), Some(color));
                } else if c.modifier.contains(Modifier::DIM) {
                    color = dim;
                }
                let mut css = String::new();
                if color != fg {
                    css += &format!("color:{color};");
                }
                if let Some(back) = back {
                    css += &format!("background:{back};");
                }
                if c.modifier.contains(Modifier::BOLD) {
                    css += "font-weight:700;";
                }
                if css.is_empty() {
                    out += &esc(&text);
                } else {
                    out += &format!("<span style=\"{css}\">{}</span>", esc(&text));
                }
            }
            out.push('\n');
        }
        out + "</pre>"
    }
}
