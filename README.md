# ridgewarden-lightning

[![CI](https://github.com/Londopy/ridgewarden-lightning/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-lightning/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-lightning)](https://github.com/Londopy/ridgewarden-lightning/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-lightning/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-lightning)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Locate lightning over an AREDN mesh, with the internet up or down, and say when it's dry lightning.**

A ridgewarden app for AREDN meshes. Sites with a VLF receiver (a loop or whip antenna into a GPS-timed sound card) run [vlfrx-tools](http://abelian.org/vlfrx-tools/), whose `vttoga` writes the arrival time of each sferic, the radio pulse a lightning strike gives off. This app reads those arrival times and sends them over the mesh as signed events, a few bytes per sferic. One site runs the solver: it matches arrivals from different sites that fit one strike, locates the strike, and logs it, so every site shows the same map. Strikes inside a watch area or near a site raise an alert, and a critical dry-lightning alert when the area's rain gauges report under 2.5 mm.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box. The core is the only part that talks to the mesh; this app reads the core's event feed and asks it to sign and log events. Spec sheet 11 of the AREDN app suite.

## Features

- Reads `vttoga` output as vlfrx-tools writes it: `H` records (`H 1795000000.123456 3.215e-02 4.71 37.5`: arrival time to the microsecond, RMS amplitude, impulse ratio, and a bearing when the receiver has crossed loops), no more strictly than vtspot reads them, and `M` records with a reference time and offset. Extended records (`-e`) are skipped. The source can be a file, `vttoga -d`'s hourly directory, or a command's output.
- Keeps each arrival's impulse ratio, vttoga's measure of how sharp the onset is (the energy just after the trigger over the energy just before: a sferic is well above 1, steady noise near it). `min_impulse` drops dull triggers such as hum bursts at the receiver, the way `min_rms` drops weak ones, and the strike page shows each arrival's ratio.
- Uses bearings in the fit. With crossed loops vttoga gives each sferic a bearing, and the solver adds it to the least-squares fit as one more equation next to the arrival times, combining time of arrival with direction finding (vtspot's own solver weighs arrival-time differences and bearings together too). A bearing from loops alone could point either way along its line, so it counts as a line through the site (`bearings = "axis"`); with a whip channel too it's a direction (`"full"`). Bearings pick between three-site groups and between the two positions three arrival times can fit, and shrink the ellipse; they can't reject a strike. Bearings that don't fit with the arrival times are left out and the times alone fix the strike, as they would in a network without loops; three arrivals that only fit that way are marked unchecked and raise no alert. One strike's check only catches a gross error (about 35 degrees and up at a 5 degree bearing error); a steady turn of the loops shows in the site's median bearing residual, and once that sets off the receiver table's warning the site's bearings are left out of every fit.
- Optional GPS check through gpsd: while the receiver has no fix, its arrivals are dropped, and the page shows the site as unlocked.
- Receiver status per site: receiving, silent (no report for 3 minutes; receivers report every minute even when quiet), GPS unlocked, unable to read vttoga's output, arrival times more than a minute off the computer's clock, timing that looks off (its median residual over ten or more strikes is further from zero than its own timing error), or bearings that look off (the same for its bearing residual, taken around the circle, past 3 degrees or its bearing error: loops not lined up the way vttoga's `-p` says), which also leaves its bearings out of fits. Also sferics and drops in the last hour, how many strikes it took part in this week, and its median residuals: the numbers to watch while you move an antenna or chase hum.
- A site name belongs to the station key that first reported it, so one station can't report as another's receiver; a network holds up to 64 sites.
- Locates strikes by time difference of arrival on a spherical earth, the same model as vtspot (great-circle paths, group velocity 0.9922 c). Arrivals are grouped only when every pair is within the light-time between the two receivers.
- Every strike has a 95% error ellipse from the fit's covariance and each receiver's configured timing and bearing errors, so it widens where the geometry is weak, such as outside the ring of receivers. The ellipse is a straight-line approximation of the fit; well outside the ring the real uncertainty bends into a band that runs further out on the far side, so take it as a guide there.
- A strike takes four or more receivers by default (`min_sites = 4`), as the spec asks: their least-squares fit has residuals (shown per receiver) that reject arrivals that don't fit one strike. A network of three receivers sets `min_sites = 3`. Three arrival times give an exact fit, as many as unknowns (position and time), so nothing in the times checks it, the ellipse is geometry and timing alone (plus any bearings), and there can be two valid answers; those strikes are marked ambiguous and don't raise alerts. Such a group is used only when it is the only three-site group that fits and no four arrivals line up; otherwise the arrivals are set aside rather than guessed at. With bearings, a three-site group is taken when it fits clearly better (by 9 in chi-square) than any other three-site group; when none fits with its bearings, they're tried again without them, and a strike found that way is marked unchecked and raises no alert.
- A map in inline SVG with no map tiles, so it works offline: strikes with ellipses colored by age, receivers and their ring, watch areas, a scale bar. Strike history, and a page per strike with its arrivals and residuals. Strikes outside the ring of receivers are marked, since that is where accuracy falls off.
- An Accuracy page for planning and checking the network: a map of the region shaded by the 95% error a strike would get at each point (the solver's own covariance, bearings included from receivers that send them, worked out before any strike), the expected accuracy inside the ring against the spec's 1 to 3 km target, the timing error the residuals of strikes fixed by arrival times alone really show compared with what's configured, the bearing residuals next to the configured bearing error, which receiver the network can least afford to lose, each watch area's worst case, and the median error of strikes actually located.
- `compare` checks located strikes against a reference set you bring (`time,lat,lon` CSV) and, with `--publish`, logs the agreement through the core: how many reference strikes inside the ring were located, the median distance, and how many were within 3 km. Every site shows the results on the Accuracy page.
- `check-source` reads the end of the configured vttoga output and says what's in it (records, bad lines, the newest arrival against the clock), and `test-alert` sends a test notice, so you can confirm alerts reach you with the internet unplugged.
- Watch areas, a polygon, a circle, or a circle around a site, with alerts to ntfy, Discord or webhooks (acknowledge links, repeats, quiet hours, maintenance windows, silences). A circle around a site (`site = "tst-cuesta"` and its own `radius_km`) alerts when lightning lands near that site; it sits on the site's receiver antenna, or for a site without a receiver on the position its AREDN node reported in the core's polls in the last eight days. A strike in an area is a warning; with under 2.5 mm of rain in the lookback it's a critical dry-lightning alert that names the rain gauge and its reading. With no fresh rain data the alert says so and never calls it dry.
- Rain from APRS weather stations (most are CWOP members) over APRS-IS, read-only, from a public server or an APRS-IS server on the mesh to keep it working offline; and from the weather app (ridgewarden-weather), whose `weather.rain` events from trusted stations carry APRS and NWS readings over the mesh. Both go through the same rules.
- `simulate` writes vttoga-style records for synthetic strikes, with bearings if you ask, so you can try everything without receivers.
- Strikes as GeoJSON (`/api/strikes.geojson`, points with the ellipse in the properties) and CSV (`/api/strikes.csv`), both `?h=1`, `3`, `24` or `168`; receiver status at `/api/receivers`.
- A busy storm delays strikes rather than stalling the app: the solver works on a copy of the arrivals off the database lock, at most ten minutes of them and five seconds of solving per round, and carries on where it stopped. Groups take at most ten other sites. How many strikes a second a Pi keeps up with hasn't been measured.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-lightning/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-lightning/releases/latest): `sudo apt install ./ridgewarden-lightning_<version>_arm64.deb` (`_amd64` on a PC). Either way the download is checked against the release key your core already trusts, `/etc/ridgewarden/lightning.toml` is written if missing, and the service starts once the core is set up. The page is on port 8796.

Every site runs this app: sites with a receiver to report, any site to see the map. Turn the solver on at one site.

### The receiver

The receiver itself is vlfrx-tools' job, and its own documentation covers the hardware and setup ([abelian.org/vlfrx-tools](http://abelian.org/vlfrx-tools/), and [abelian.org/rpvlf](http://abelian.org/rpvlf/) for a Raspberry Pi). In short: an antenna and preamp into a sound card read by `vtcard`, a GPS 1PPS into a second channel so `vttime` can timestamp the samples to the microsecond, then `vttoga` to trigger on sferics and write their arrival times. For this app, run vttoga with an output directory:

```sh
vttoga -B -F 4000,17000 -i 5 -r 20 -d /var/lib/vlf/toga <timestamped input>
```

vttoga needs a trigger level to start from (`-i` on the impulse ratio, or `-a` on amplitude); `-r 20` then moves it every minute toward about 20 sferics a second. Distant storms are heard too, so keep the rate modest. With two crossed loops, vttoga's `-p` option tells it which channel is which loop (and which is a whip, if there is one), and its H records then end with a bearing; set `bearings` in `[receiver]` to match. vlfrx-tools is under a BSD-style license.

Site choice matters more than anything else here: VLF receivers want to be well away from power lines, which rules out most mountaintop radio sites. Expect to spend time on antenna placement and hum.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-lightning`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-lightning ridgewarden-lightning --config /etc/ridgewarden/lightning.toml <command>`.

## Usage

```sh
ridgewarden-lightning init --config /etc/ridgewarden/lightning.toml
ridgewarden-lightning run  --config /etc/ridgewarden/lightning.toml
```

The page is on `http://<pi>:8796/`. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

### Trying it without receivers

`simulate` writes one H record per receiver for each strike, with Gaussian timing noise, appending to `<out>/<name>.toga`:

```sh
ridgewarden-lightning simulate --out /tmp/toga \
  --receiver tst-north=35.55,-120.70 --receiver tst-east=35.30,-120.35 \
  --receiver tst-south=35.05,-120.62 --receiver tst-west=35.32,-120.95 \
  --strike 35.35,-120.60 --strike 35.20,-120.80 --sigma-us 5 --bearings axis
```

Run one app per receiver (each with `receiver.name` and `toga_file = "/tmp/toga/<name>.toga"`; they can share one core), turn the solver on in one of them with a short `settle_s`, and run `simulate` after they've started. The strikes appear on the map once they settle. `--bearings axis` (or `full`) adds a bearing to each record with `--bearing-sigma-deg` of noise, 2 by default; leave it out for receivers without loops.

### Checking a site and the network

```sh
ridgewarden-lightning check-source --config /etc/ridgewarden/lightning.toml   # is vttoga's output there and sane?
ridgewarden-lightning test-alert   --config /etc/ridgewarden/lightning.toml   # do alerts reach you?
ridgewarden-lightning compare      --config /etc/ridgewarden/lightning.toml \
  --reference storm.csv --source "reference network, storm of 2026-11-19" --publish
```

`compare` reads `time,lat,lon` lines (RFC 3339 or unix seconds; extra columns and a header are fine) and matches each to the strike nearest in time within `--window-ms` (50 by default). Run it where the solver's strikes are, which is every site.

### Using the strikes elsewhere

Strikes are logged as `lightning.strikes` events in every core's main log, so any ridgewarden app can follow them from the feed. The payload is `{"velocity_factor": 0.9922, "strikes": [...]}`, each strike `{"id", "t_us", "lat", "lon", "major_km", "minor_km", "az_deg", "rms_us", "ambiguous", "unchecked", "sites": [[site, t_us, resid_us, impulse, bearing_deg, bearing_resid_deg], ...]}`: time of the strike in unix microseconds, position in degrees, the 95% ellipse's semi-axes and bearing, the RMS residual in arrival time (null for an exact fit: three arrival times and no bearing in the fit), whether another position fits as well, whether it's three arrival times that only fit once their bearings were left out (0.1.0 didn't write this), and each receiver's arrival, residual, impulse ratio, bearing and bearing residual (the last three null where there's none; 0.1.0 wrote only the first three). A seventh element, `true`, follows a bearing that was left out of the fit. Ambiguous and unchecked strikes raise no alert. An event holds as many strikes as fit under the core's 128 KiB limit, 200 at most. A Rust app can use `ridgewarden_lightning::store::parse_strike`, which checks everything. Outside the mesh, `/api/strikes.csv` and `/api/strikes.geojson` give the same data.

## Configuration

`/etc/ridgewarden/lightning.toml` (`init` writes a commented one):

```toml
[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # lets this app log events via the core

[receiver]                       # only at sites with a receiver
lat = 35.3500                    # the antenna, surveyed to a few metres
lon = -120.6500
timing_sigma_us = 20             # one arrival's timing error (1 sigma), see below
toga_dir = "/var/lib/vlf/toga"   # or toga_file = "...", or toga_command = "..."
gpsd = "127.0.0.1:2947"          # optional: drop arrivals without a GPS fix
bearings = "axis"                # crossed loops: "axis", "full" with a whip, or "off"
bearing_sigma_deg = 5            # one bearing's error (1 sigma), vtspot's default
# name = "tst-cuesta"            # defaults to this site's node name
# min_rms = 0.0                  # drop weaker sferics
# min_impulse = 0.0              # drop sferics with a duller onset (steady noise is near 1)
# batch_s = 10                   # seconds between reports to the core

[solver]                         # at one site
enabled = true
min_sites = 4                    # the default; 3 for a network of three receivers
velocity_factor = 0.9922         # VLF group velocity as a fraction of c (vtspot's default)
settle_s = 180                   # longer than the core's [sync] interval_s
max_range_km = 300               # ignore solutions further out than this
max_axis_km = 30                 # drop strikes whose ellipse is longer than this
max_chi2 = 9                     # with 4+ sites, chi-square per degree of freedom

[rain]
from_aprs_is = true                  # read the stations from APRS-IS
from_weather_app = true              # and use rain the weather app logs on the mesh
aprs_is = "rotate.aprs2.net:14580"   # or an APRS-IS server on the mesh
login = "N0CALL"                     # read-only, no passcode
lookback_h = 24                      # 1 or 24: APRS reports rain for the last hour and 24 hours
max_age_min = 30                     # older reports are no data
dry_mm = 2.5

[[watch]]
name = "Backcountry"
polygon = [[35.55, -120.95], [35.62, -120.45], [35.30, -120.40]]   # [lat, lon]
rain_stations = ["CW0000"]

[[watch]]
name = "Near the hub"
center = [35.30, -120.66]
radius_km = 10

[[watch]]
name = "Near Cuesta"
site = "tst-cuesta"              # a node name: around its receiver, or its node's own position
radius_km = 15
```

`timing_sigma_us` is the whole error of one arrival time, not just the GPS: a GPS PPS is good to a microsecond or so, but vttoga's group-arrival estimate on a real sferic is usually the larger part. 20 us is vtspot's own default. It scales every ellipse directly (20 us is about 6 km of path), so once you have residuals from four or more sites, set it to what they show.

Bearings only matter at a receiver with crossed loops; without them vttoga writes none and these settings do nothing. `"axis"` is always safe: it reads every bearing as a line through the site, which is all two loops can tell. Use `"full"` only when vttoga's `-p` includes a whip (E-field) channel, and `"off"` for loops you don't trust yet. `bearing_sigma_deg` weighs bearings against arrival times in the fit and sizes the ellipse, like `timing_sigma_us` does; neither can be set below 1 us or 0.5 degrees, and the solver takes a smaller claim from another station as those floors. One strike's check only catches a gross bearing error, about 35 degrees and up at 5 degrees each: then the solver leaves that strike's bearings out and fixes it from the arrival times alone. A steady turn of 10 or 20 degrees passes that check and makes the ellipses too small, so the receiver table watches each site's median bearing residual over the week: once ten or more of them sit further from zero than 3 degrees or the site's bearing error, it warns, and the solver leaves that site's bearings out of every fit until its newer residuals bring the median back. Fix its loop alignment in vttoga's `-p`, or set `bearings = "off"` there until you have.

`min_sites` is 4, the spec's "four or more sites": with four the fit has residuals to check, while sferics from different storms can line up by chance at three sites. A network that only has three receivers sets it to 3 and gets three-site strikes; bearings pick between three-site groups and shrink the ellipse, but they can't reject one, so three arrivals whose bearings don't fit are marked unchecked and raise no alert. The map page says when the solver has fewer receivers than `min_sites`.

A circle around a site follows the site: its receiver's surveyed antenna if it has reported in the last eight days, otherwise the latitude and longitude its AREDN node reported in the last eight days, which the core logs with every poll (including the nodes it crawls). Until either is known, the area shows as waiting and raises nothing.

Strikes are solved `settle_s` after they happen, so every site's report has synced. The core syncs every 120 s by default; on a lightning network a shorter `[sync] interval_s` (say 15) gives faster alerts, and `settle_s` can come down with it. Arrivals that arrive later than `settle_s` aren't used.

The rain stations are APRS callsigns (CWOP IDs look like `CW0000` or `EW0000`), or NWS station IDs (like `KTST`) for readings that come from the weather app; the alert says which source a reading came through. NWS observations give rain for the last hour only, so they count with `lookback_h = 1`. Every station in an area counts: one wet gauge makes the area not dry, and the alert names the station and reading that decided it. A wet reading keeps counting for the whole lookback even if that gauge goes quiet; a dry one only counts while it is fresher than `max_age_min`. If the rain data goes stale while a dry-lightning alert is open, the alert stays open and says so rather than resolving.

### Alerts

Set alerts up on one site only, or every site sends the same one. They work like the other ridgewarden apps' alerts:

```toml
[alerts]
ntfy = "https://ntfy.sh/your-mesh-topic"      # or a self-hosted ntfy on the mesh
public_url = "http://tst-hub-pi.local.mesh:8796" # for acknowledge links
quiet_hours = "22-6"                            # UTC; only critical alerts go out
repeat_hours = 4
```

An alert stays open while its area has had strikes in the last `watch_window_min` (30 by default) and resolves after.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`. Arrival times, positions and strikes from the feed are checked before use; anything malformed is skipped.

## Limits

- It has not run against real receivers. The vttoga format is taken from vlfrx-tools' source (`vttoga.c`'s output and `vtspot.c`'s reader), and the pipeline is tested with simulated records; real sferics will need the timing error and vttoga's trigger settings tuned per site.
- Bearings and impulse ratios have only been simulated. Real crossed loops have site errors that change with direction, which one `bearing_sigma_deg` per site doesn't model, and no `min_impulse` has been tried against real noise; set both from what each site's receiver table shows.
- No automatic comparison with public lightning maps; `compare` takes a reference file you may use instead. Blitzortung's terms rule it out: its data is for private and entertainment purposes only, may not be used for storm warning systems, and its raw data is only for its participants or those it allows ([blitzortung.org, "Important information for our visitors"](https://www.blitzortung.org/en/contact.php#contribution_1)). WWLLN's data is copyrighted and sold: archives at nominal cost, real time through a commercial reseller, 10-minute data for research by arrangement, free only to sites that host a WWLLN sensor ([wwlln.net, "WWLLN Data Available"](https://wwlln.net/#available)). NOAA's GOES lightning mapper (GLM) is free to use ([NOAA on AWS](https://registry.opendata.aws/noaa-goes/)) but comes as NetCDF files, a new dependency to read, at 8 to 14 km resolution ([goes-r.gov](https://www.goes-r.gov/spacesegment/glm.html)). Xweather sells lightning data through a pay-as-you-go API whose first 15,000 accesses a month are free in the US and Canada, a lightning request counting as ten ([xweather.com](https://www.xweather.com/pricing/weather-api-pay-as-you-go)); it needs an account, an API key and a read of its terms, so it isn't built in.
- No Blitzortung feed. Blitzortung's stations run its own detector, which sends about a millisecond of each signal, sampled at over 500 kHz, to its servers ([blitzortung.org, "Cover your area"](https://www.blitzortung.org/en/cover_your_area.php)); it documents no way to send data from other receivers, so a vlfrx-tools receiver and this app can't contribute. A site that wants to help both networks runs a Blitzortung receiver next to its VLF receiver.
- Choosing electrically quiet sites, antennas and hum cancellation is fieldwork; the receiver table's rates, drops and residuals help judge a site, but can't pick one.
- The dry-lightning check uses the strike's located center, not its ellipse.
- A circle around a site without a receiver sits wherever the node's owner put it in AREDN's location settings.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-lightning
cd ridgewarden-lightning
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
