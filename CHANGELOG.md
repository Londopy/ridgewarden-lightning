# Changelog

All notable changes to ridgewarden-lightning are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Each arrival keeps vttoga's impulse ratio and bearing: they go to the core with the arrival time and show on the strike page. `min_impulse` drops sferics with a dull onset, such as hum bursts.
- Bearings from crossed loops go into the fit next to the arrival times (`bearings = "axis"`, `"full"` or `"off"`, and `bearing_sigma_deg`). They pick between three-site groups and shrink the ellipse; they can't reject one. One strike's check only catches a gross bearing error (about 35 degrees and up at 5 degrees each): those bearings are left out and the times alone fix the strike, and three arrivals that only fit that way are marked unchecked and raise no alert. The Accuracy page counts bearings too.
- The receiver table shows each site's median bearing residual, taken around the circle and left-out bearings included, and warns when its loops look turned from where vttoga's `-p` says. Such a site's bearings are left out of every fit until its median comes back, since a steady turn of 10 to 35 degrees passes each strike's check but makes the ellipses too small.
- A watch area can be a circle around a site (`site` and `radius_km`), on the site's receiver antenna or, without a receiver, its node's position from the core's polls, to alert when lightning lands near that site.
- `simulate --bearings axis` (or `full`) adds bearings to the simulated records.
- The map page says when the solver has fewer receivers than `min_sites`.

### Changed

- `min_sites` is 4 by default, as the spec asks; a network of three receivers sets it to 3.
- A three-site group with a bearing is taken when it fits clearly better than every other three-site group, instead of only when it is the only one that fits.
- Strike events list each arrival's impulse ratio, bearing and bearing residual after its residual, with `true` after a bearing that was left out of the fit, carry `unchecked`, and are split by size to stay under the core's 128 KiB limit. The CSV and GeoJSON exports carry `unchecked` too.
- A receiver can't claim a timing error under 1 us or a bearing error under 0.5 degrees: the config refuses it, and a smaller claim in a report is taken as the floor.
- The Accuracy page's timing estimate only counts strikes fixed by their arrival times alone, since bearings pull a fit off the times, and the bearing residuals' RMS shows next to it.

### Fixed

- Strike events with many sites and long names could go over the core's size limit and stall the solver.
- Two watch areas with the same name are refused; the name keys the alert.
- The README's vttoga command lacked the trigger level vttoga requires (`-i` or `-a`).
- The README's claims about Blitzortung, WWLLN, GLM and NLDN link their sources and say what each allows; Xweather's free API allowance is noted, and the unmeasured "scales to a county" is gone.
- `H` records are read no more strictly than vtspot reads them: an unusable impulse ratio sign or bearing no longer drops the arrival. The claim that vtspot accepts `M` records is gone; they're read as before.
- A bearing a hair under 360 degrees no longer drops its arrival or strike as a bad record.
- If logging one event of a round's strikes failed, the next round could log the strikes before it again.
- Node positions from the core's polls are pruned after eight days.

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-lightning`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Reads vlfrx-tools' `vttoga` output (H and M records) from a file, its `-d` hourly directory, or a command, and reports each site's arrivals to the core as signed telemetry events.
- Optional gpsd check: arrivals are dropped while the GPS has no fix, and the page says so.
- A solver that groups arrivals from different sites into strikes and locates them on a sphere by time difference of arrival, with a 95% error ellipse from each receiver's timing error.
- Least-squares fits with residuals from four or more receivers; exact fits from three, flagged when a second position fits as well. Three-site groups are only used when nothing else could explain the arrivals.
- The solver works off the database lock with a time budget per round, and a site name belongs to the station that first reported it.
- Located strikes logged through the core, so every site has the same map and history.
- A strike map in inline SVG (no map tiles, works offline) with error ellipses, watch areas and receivers; strike history; a page per strike with each arrival's residual; receiver status per site.
- Watch areas (polygon or circle) with alerts, and a critical dry-lightning alert when the area's APRS weather stations report under 2.5 mm of rain; missing rain data is said plainly and never counted as dry.
- Rain from APRS weather stations (CWOP) over APRS-IS, from a public server or one on the mesh, and from the weather app's `weather.rain` events (APRS and NWS readings) from trusted stations. A wet reading counts for the whole lookback, a dry one only while fresh, and an open dry-lightning alert stays open when the rain data goes stale.
- `simulate`, which writes vttoga-style records for synthetic strikes so the whole chain can run without receivers.
- Strikes as GeoJSON at `/api/strikes.geojson`, as CSV at `/api/strikes.csv`, and receiver status at `/api/receivers`.
- An Accuracy page: the region shaded by the expected 95% error from the receivers' geometry and timing, the expected accuracy inside the ring, the timing error the residuals show, the receiver the network can least afford to lose, each watch area's worst case, and logged comparisons with reference data.
- `compare`, which matches located strikes against a reference CSV and can log the agreement through the core.
- `check-source`, which says what the end of the configured vttoga output holds, and `test-alert`, which sends a test notice.
- Receiver warnings for arrival times off the computer's clock and for timing that the residuals show is off; strikes taken part in and median residual per site.
- Strikes outside the ring of receivers are marked, and the ring is drawn on the map.
- The APRS weather parser also reads wind direction, sustained wind and gust (`Wx::wind_dir_deg`, `wind_mph`, `gust_mph`) for other apps to use.

[Unreleased]: https://github.com/Londopy/ridgewarden-lightning/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-lightning/releases/tag/v0.1.0
