"""Compare the WeeWX adapter with its existing PyEphem provider."""

import subprocess
import sys

import pytest

pytest.importorskip('ephem')
import weewx.almanac
import weewx.units
import weewxdb.almanac


@pytest.mark.parametrize('body', ['sun', 'moon'])
@pytest.mark.parametrize('stamp', [1704110400, 1718928000, 1734782400])
@pytest.mark.parametrize('latitude,horizon,center', [(52, 0, False), (52, -6, True),
                                                    (52, -12, True), (52, -18, True),
                                                    (69.65, 0, False)])
def test_almanac_values_without_pyephem_calls(monkeypatch, body, stamp, latitude, horizon, center):
    almanac = weewx.almanac.Almanac(stamp, latitude, 13, altitude=155, horizon=horizon)
    monkeypatch.setattr(weewx.almanac, 'almanacs', [weewx.almanac.PyEphemAlmanacType()])
    attributes = ['az', 'alt', 'ra', 'dec', 'g_ra', 'g_dec', 'radius', 'earth_distance', 'phase',
                  'azimuth', 'altitude', 'topo_ra', 'topo_dec', 'geo_ra', 'geo_dec', 'radius_size',
                  'rise', 'set', 'transit', 'next_rising', 'next_setting', 'previous_rising',
                  'previous_setting', 'next_transit', 'previous_transit', 'next_antitransit',
                  'previous_antitransit', 'visible']
    if body == 'moon':
        attributes += ['moon_fullness']
    expected_body = getattr(almanac, body)(use_center=center)
    expected = {name: getattr(expected_body, name) for name in attributes}
    monkeypatch.setattr(weewx.almanac, 'almanacs', [weewxdb.almanac.NativeAlmanac()])
    monkeypatch.delattr(weewx.almanac, 'ephem')
    actual_body = getattr(almanac, body)(use_center=center)
    for name, reference in expected.items():
        actual = getattr(actual_body, name)
        time_value = False
        if isinstance(reference, weewx.units.ValueHelper):
            assert actual.value_t[1:] == reference.value_t[1:]
            time_value = reference.value_t[1] == 'unix_epoch'
            actual, reference = actual.raw, reference.raw
        if reference is None:
            assert actual is None, name
        else:
            tolerance = 0.5 if name == 'visible' or time_value else 2e-5
            assert actual == pytest.approx(reference, abs=tolerance, rel=0), (body, name)


@pytest.mark.parametrize('stamp', [1704110400, 1718928000, 1734782400])
def test_lunar_almanac_and_sidereal(monkeypatch, stamp):
    almanac = weewx.almanac.Almanac(stamp, 52, 13)
    names = ['moon_index', 'moon_phase', 'moon_fullness', 'sidereal_time', 'sidereal_angle'] + [
        direction + '_' + phase for direction in ['next', 'previous']
        for phase in ['new_moon', 'first_quarter_moon', 'full_moon', 'last_quarter_moon']]
    monkeypatch.setattr(weewx.almanac, 'almanacs', [weewx.almanac.PyEphemAlmanacType()])
    expected = {name: getattr(almanac, name) for name in names}
    monkeypatch.setattr(weewx.almanac, 'almanacs', [weewxdb.almanac.NativeAlmanac()])
    monkeypatch.delattr(weewx.almanac, 'ephem')
    for name, reference in expected.items():
        actual = getattr(almanac, name)
        if isinstance(reference, weewx.units.ValueHelper):
            assert actual.value_t[1:] == reference.value_t[1:]
            assert actual.raw == pytest.approx(reference.raw, abs=0.05 if reference.value_t[1] == 'unix_epoch' else 1e-9, rel=0)
        else:
            assert actual == pytest.approx(reference, abs=1e-9) if isinstance(reference, float) else actual == reference


def test_registration_keeps_custom_almanac_priority(monkeypatch):
    custom = weewx.almanac.AlmanacType()
    standard = weewx.almanac.PyEphemAlmanacType()
    monkeypatch.setattr(weewx.almanac, 'almanacs', [custom, standard])
    weewxdb.almanac.register()
    weewxdb.almanac.register()
    assert weewx.almanac.almanacs[0] is custom
    assert type(weewx.almanac.almanacs[1]) is weewxdb.almanac.NativeAlmanac
    assert weewx.almanac.almanacs[2] is standard


def test_report_almanac_without_pyephem():
    program = '''
import sys
sys.modules['ephem'] = None
import weewx.almanac
import weewxdb.almanac
weewxdb.almanac.register()
almanac = weewx.almanac.Almanac(1704110400, 52, 13)
assert almanac.hasExtras
assert almanac.sun.rise.raw is not None
assert almanac.moon.set.raw is not None
assert almanac(horizon=-18).sun(use_center=True).rise.raw < almanac.sun.rise.raw
assert 0 <= almanac.moon_index <= 7
assert sys.modules['ephem'] is None
'''
    subprocess.run([sys.executable, '-c', program], check=True, capture_output=True, text=True)
