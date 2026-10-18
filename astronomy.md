# Solar and lunar astronomy

The Rust core calculates the Sun and Moon without a Python ephemeris or a C
astronomy library. Opening the weedb driver registers an almanac provider for
WeeWX reports. Existing custom almanacs keep their priority.

## WeeWX attributes

Supported solar/lunar attributes include rise, set, transit, next/previous
rising, setting, transit and antitransit, visible, visible_change, az/alt,
topocentric and geocentric right ascension/declination, angular radius,
earth_distance and phase. The corresponding ValueHelper names azimuth,
altitude, topo_ra/dec, geo_ra/dec and radius_size are supported.

The almanac also supports sunrise, sunset, sidereal_time, sidereal_angle,
moon_phase, moon_index, moon_fullness and next/previous new moon, first quarter,
full moon and last quarter. WeeWX's separate moon_fullness and phase models
retain their different definitions.

Twilight uses the normal WeeWX horizon override, for example
`almanac(horizon=-18).sun(use_center=True).rise`. Civil, nautical and
astronomical twilight use -6, -12 and -18 degrees. Observation pressure and
temperature affect refraction. Polar rise/set results are unavailable when
the event does not occur in the search interval.

Planets, comets, satellites, equinoxes, solstices, J2000 astrometric coordinates
and other PyEphem attributes are outside the Rust contract. Existing PyEphem
providers remain available for these attributes when installed. They are
unavailable in a Rust-only installation.

## Python API

```python
import weewxdb

position = weewxdb.astronomy(1704110400, 52, 13, elevation=155, body='moon')
dawn_dusk = weewxdb.astronomy(1704067200, 52, 13, body='sun',
                              operation='events', horizon=-18, center=True)
phases = weewxdb.astronomy(1704067200, 0, 0, operation='phases',
                           stop=1735689600)
```

| Parameter | Default | Meaning |
| --- | --- | --- |
| timestamp | Required | Unix seconds, including fractions |
| latitude / longitude | Required | Degrees north / east |
| elevation | 0 | Meters above sea level |
| body | sun | sun or moon |
| pressure | 1010 | hPa; zero disables refraction |
| temperature | 15 | Celsius |
| horizon | 0 | Degrees above the astronomical horizon |
| operation | position | position, events, phases or sidereal |
| start | timestamp | First search timestamp |
| stop | Start plus one day for events, 32 days for phases | Last search timestamp |
| center | False | Use the body's center instead of its upper limb for rises/sets |

Position results are JSON-compatible dictionaries containing altitude, azimuth,
right_ascension, declination, geocentric_ra/dec, hour_angle,
geocentric_longitude/latitude, angular_radius, phase_angle, illumination,
moon_fullness and distance_au. Angles are degrees. Illumination and fullness are
percentages. Equatorial right_ascension/declination are topocentric apparent
coordinates. Geocentric longitude/latitude are the reference model's mean
ecliptic coordinates. Event results contain lists rises, sets, transits and
antitransits in Unix seconds, plus always_up and never_up. Phase events are
chronological `[name, timestamp]` pairs. Sidereal returns a local apparent
sidereal angle in degrees.

Searches cover at most 370 days. The reference model accepts years -1000
through 2900. Independent comparisons currently cover 1900 through 2100;
that validation does not establish uniform accuracy throughout the full
model range. Ordinary event comparisons allow 0.5 seconds against PyEphem,
phase-event comparisons 0.05 seconds. Apparent alt/az comparisons allow
0.00002 degrees because PyEphem stores them as 32-bit floats.

The SQL functions astro_position and astro_events return the same JSON
structures. Their signatures are listed in the [SQL reference](reference.md).
For example:

```sql
SELECT json_extract(astro_position('sun', dateTime, 52, 13, 155), '$.altitude')
FROM archive WHERE dateTime > 1704067200;
```

## Numerical sources

The numerical reference is [PyEphem](https://github.com/brandon-rhodes/pyephem)
at commit `6be1e55540fd32aaeff272733190128155e8a35c`. Rust implements the
VSOP87 Earth/Sun terms, Moshier lunar series, nutation, delta-T, parallax,
refraction and event searches. The coefficient import is reproducible with
`tools/import_ephemeris.py PATH_TO_PINNED_PYEPHEM_CHECKOUT`. Generated numeric
tables ship in the crate; builds need no external ephemeris download.
The original MIT notice is in [vendor/LICENSE.ephem](vendor/LICENSE.ephem).

The [measurements](results/README.md) distinguish fused database calculations
from individual astronomy calls. PyEphem already performs its numerical work
in C, so replacing it does not itself imply a speedup.
