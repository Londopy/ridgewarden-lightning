# Changelog

All notable changes to ridgewarden-lightning are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

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

[Unreleased]: https://github.com/Londopy/ridgewarden-lightning/commits/main
