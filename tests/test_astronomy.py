"""Compare the Rust ephemeris with the independent PyEphem reference."""

import math
import subprocess
import sys

import pytest

import weewxdb

ephem = pytest.importorskip('ephem')


def observer(timestamp, latitude, longitude, elevation, pressure=1010.0, horizon=0):
    result = ephem.Observer()
    result.lat = math.radians(latitude)
    result.lon = math.radians(longitude)
    result.elevation = elevation
    result.pressure = pressure
    result.horizon = math.radians(horizon)
    result.date = timestamp / 86400 + 25567.5
    return result


@pytest.mark.parametrize('body', ['sun', 'moon'])
@pytest.mark.parametrize('latitude,longitude,elevation', [(52, 13, 155), (-33, 151, 800),
                                                         (0, -78, 2800), (69.65, 18.96, 0),
                                                         (-90, 0, 0), (90, 0, 100)])
@pytest.mark.parametrize('timestamp', [-2208988800, 0, 1704110400, 1710936000, 1718928000,
                                       1729512000, 1734782400, 4102444800])
def test_positions_match_pyephem(body, latitude, longitude, elevation, timestamp):
    actual = weewxdb.astronomy(timestamp, latitude, longitude, elevation=elevation, body=body)
    site = observer(timestamp, latitude, longitude, elevation)
    expected = getattr(ephem, body.title())(site)
    for name, reference in [('altitude', expected.alt), ('azimuth', expected.az),
                             ('right_ascension', expected.ra), ('declination', expected.dec)]:
        difference = (actual[name] - math.degrees(reference) + 180) % 360 - 180
        # PyEphem stores alt/az as f32. This covers their maximum rounding
        # step in degrees; equatorial coordinates retain double precision.
        assert abs(difference) < (2e-5 if name in ['altitude', 'azimuth'] else 1e-7)
    assert actual['distance_au'] == pytest.approx(expected.earth_distance, rel=6e-8)
    assert actual['illumination'] == pytest.approx(expected.phase, abs=5e-6)


@pytest.mark.parametrize('body', ['sun', 'moon'])
@pytest.mark.parametrize('latitude,longitude', [(52, 13), (-33, 151), (69.65, 18.96)])
@pytest.mark.parametrize('timestamp', [1711843200, 1718928000, 1734782400])
@pytest.mark.parametrize('horizon,center', [(0, False), (-6, True), (-12, True), (-18, True)])
def test_events_and_twilight_match_pyephem(body, latitude, longitude, timestamp, horizon, center):
    stop = timestamp + 86400
    actual = weewxdb.astronomy(timestamp, latitude, longitude, body=body,
                               operation='events', stop=stop, horizon=horizon, center=center)
    site = observer(timestamp, latitude, longitude, 0, horizon=horizon)
    for key, method in [('rises', 'next_rising'), ('sets', 'next_setting'), ('transits', 'next_transit')]:
        reference = []
        site.date = timestamp / 86400 + 25567.5
        for _ in range(3):
            try:
                function = getattr(site, method)
                if key == 'transits':
                    date = function(getattr(ephem, body.title())())
                else:
                    date = function(getattr(ephem, body.title())(), use_center=center)
            except ephem.CircumpolarError:
                break
            value = (float(date) - 25567.5) * 86400
            if value >= stop:
                break
            reference.append(value)
            site.date = float(date) + 1 / 86400
        assert len(actual[key]) == len(reference), (body, key, actual, reference)
        for a, b in zip(actual[key], reference):
            assert abs(a - b) < 0.5


def test_lunar_phase_events_match_pyephem():
    start = 1704067200
    phases = weewxdb.astronomy(start, 0, 0, operation='phases', stop=start + 365 * 86400)
    methods = {'new_moon': 'next_new_moon', 'first_quarter': 'next_first_quarter_moon',
               'full_moon': 'next_full_moon', 'last_quarter': 'next_last_quarter_moon'}
    for name, timestamp in phases:
        before = timestamp - 86400
        expected = (float(getattr(ephem, methods[name])(before / 86400 + 25567.5)) - 25567.5) * 86400
        assert abs(timestamp - expected) < 0.05
    assert 48 <= len(phases) <= 52


def test_astronomy_works_without_weewx_or_pyephem_imports():
    program = '''
import sys
sys.modules['ephem'] = None
sys.modules['weewx'] = None
import weewxdb
position = weewxdb.astronomy(1704110400, 52, 13, body='sun')
assert 14 < position['altitude'] < 15
moon = weewxdb.astronomy(1704110400, 52, 13, body='moon')
assert 73 < moon['illumination'] < 75
assert sys.modules['ephem'] is None
'''
    subprocess.run([sys.executable, '-c', program], check=True, capture_output=True, text=True)


def test_grazing_horizon_does_not_skip_short_visibility():
    stamp = 1710936000
    site = observer(stamp, 52, 13, 0, pressure=0)
    transit = site.next_transit(ephem.Sun())
    site.date = transit
    peak = math.degrees(ephem.Sun(site).alt)
    center = (float(transit) - 25567.5) * 86400
    horizon = peak - 0.0001
    actual = weewxdb.astronomy(center - 3600, 52, 13, pressure=0, horizon=horizon,
                               operation='events', stop=center + 3600, center=True)
    assert len(actual['rises']) == len(actual['sets']) == 1
    assert 0 < actual['sets'][0] - actual['rises'][0] < 120
    for value in actual['rises'] + actual['sets']:
        site.date = value / 86400 + 25567.5
        assert math.degrees(ephem.Sun(site).alt) == pytest.approx(horizon, abs=2e-5)
