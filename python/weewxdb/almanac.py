"""WeeWX solar and lunar almanac calculations backed by the Rust ephemeris."""

import functools
import math

import weeutil.weeutil
import weewx
import weewx.almanac
import weewx.units

from . import astronomy


@functools.lru_cache(maxsize=128)
def calculate(observer, body, operation, start=None, stop=None, center=False):
    timestamp, latitude, longitude, elevation, pressure, temperature, horizon = observer
    return astronomy(timestamp, latitude, longitude, elevation, body,
                             pressure, temperature, horizon, operation, start, stop, center)


def observer_key(almanac):
    return (almanac.time_ts, almanac.lat, almanac.lon, almanac.altitude,
            almanac.pressure, almanac.temperature, almanac.horizon)


# Equinoxes and solstices as PyEphem finds them: where the Sun's apparent
# geocentric right ascension reaches 0 (vernal equinox), 90 (summer
# solstice), 180 or 270 degrees. Each entry is the (motion, offset) pair of
# PyEphem's holiday() call for that name, in radians.
SEASONS = {
    'previous_vernal_equinox': (-2 * math.pi, 0), 'next_vernal_equinox': (2 * math.pi, 0),
    'previous_summer_solstice': (-2 * math.pi, 1.5 * math.pi), 'next_summer_solstice': (2 * math.pi, 1.5 * math.pi),
    'previous_autumnal_equinox': (-2 * math.pi, math.pi), 'next_autumnal_equinox': (2 * math.pi, math.pi),
    'previous_winter_solstice': (-2 * math.pi, 0.5 * math.pi), 'next_winter_solstice': (2 * math.pi, 0.5 * math.pi),
    'previous_equinox': (-math.pi, 0), 'next_equinox': (math.pi, 0),
    'previous_solstice': (-math.pi, 0.5 * math.pi), 'next_solstice': (math.pi, 0.5 * math.pi),
}


def season(almanac, motion, offset):
    """Return the Dublin Julian date of the equinox or solstice described by
    one entry of SEASONS, searched from the almanac's time like PyEphem's
    holiday().
    """
    def right_ascension(stamp):
        key = (stamp,) + observer_key(almanac)[1:]
        return math.radians(calculate(key, 'sun', 'position')['geocentric_ra'])

    def distance(stamp):
        # Distance in right ascension to the nearest multiple of 45 degrees.
        return (right_ascension(stamp) + math.pi / 8) % (math.pi / 4) - math.pi / 8

    angle = motion - (right_ascension(almanac.time_ts) + offset) % motion
    if abs(angle) < math.radians(1 / 3600) / 360:
        angle = motion
    # Secant iteration from a first guess one hour wide, to 0.1 seconds.
    x0 = almanac.time_ts + 365.25 * 86400 * angle / (2 * math.pi)
    x1 = x0 + 3600
    f0, f1 = distance(x0), distance(x1)
    while f1 and abs(x1 - x0) > 0.1 and f1 != f0:
        x0, x1 = x1, x1 + (x1 - x0) / (f0 / f1 - 1)
        f0, f1 = f1, distance(x1)
    return x1 / 86400 + 25567.5


def helper(almanac, value, unit, group, context='ephem_day'):
    return weewx.units.ValueHelper(weewx.units.ValueTuple(value, unit, group), context=context,
                                   formatter=almanac.formatter, converter=almanac.converter)


def register():
    if any(type(provider) is NativeAlmanac for provider in weewx.almanac.almanacs):
        return
    # Custom almanacs retain their order. Install immediately before the
    # first standard provider, which remains available for other bodies.
    index = next((i for i, provider in enumerate(weewx.almanac.almanacs)
                  if type(provider) in [weewx.almanac.PyEphemAlmanacType,
                                        weewx.almanac.WeeutilAlmanacType]), len(weewx.almanac.almanacs))
    weewx.almanac.almanacs.insert(index, NativeAlmanac())


class NativeAlmanac(weewx.almanac.AlmanacType):
    @property
    def hasExtras(self):
        return True

    def get_almanac_data(self, almanac_obj, attr):
        if attr in ['sun', 'moon']:
            return Body(almanac_obj, attr)
        if attr in ['sunrise', 'sunset']:
            return getattr(Body(almanac_obj, 'sun'), 'rise' if attr == 'sunrise' else 'set')
        if attr == 'moon_fullness':
            return int(calculate(observer_key(almanac_obj), 'moon', 'position')['moon_fullness'] + 0.5)
        if attr in ['moon_phase', 'moon_index'] or any(
                attr == direction + '_' + phase for direction in ['previous', 'next']
                for phase in ['new_moon', 'first_quarter_moon', 'full_moon', 'last_quarter_moon']):
            stamp = almanac_obj.time_ts
            phases = calculate(observer_key(almanac_obj), 'moon', 'phases', stamp - 32 * 86400, stamp + 32 * 86400)
            if attr in ['moon_phase', 'moon_index']:
                before = max(value for name, value in phases if name == 'new_moon' and value < stamp)
                after = min(value for name, value in phases if name == 'new_moon' and value > stamp)
                index = int((stamp - before) / (after - before) * 8 + 0.5) & 7
                return index if attr == 'moon_index' else almanac_obj.moon_phases[index]
            direction, phase = attr.split('_', 1)
            phase = phase.replace('_quarter_moon', '_quarter')
            values = [value for name, value in phases if name == phase]
            value = min(v for v in values if v > stamp) if direction == 'next' else max(v for v in values if v < stamp)
            return helper(almanac_obj, value / 86400 + 25567.5, 'dublin_jd', 'group_time', 'ephem_year')
        if attr in SEASONS:
            return helper(almanac_obj, season(almanac_obj, *SEASONS[attr]), 'dublin_jd', 'group_time', 'ephem_year')
        if attr in ['sidereal_time', 'sidereal_angle']:
            value = calculate(observer_key(almanac_obj), 'sun', 'sidereal')
            return value if attr == 'sidereal_time' else helper(almanac_obj, value, 'degree_compass', 'group_direction')
        raise weewx.UnknownType(attr)


class Body:
    def __init__(self, almanac, body):
        self.almanac = almanac
        self.heavenly_body = body
        self.use_center = False

    def __call__(self, use_center=False):
        self.use_center = use_center
        return self

    def event_data(self, stamp, previous=False):
        start, stop = (stamp - 2 * 86400, stamp) if previous else (stamp, stamp + 2 * 86400)
        return calculate(observer_key(self.almanac), self.heavenly_body, 'events', start, stop, self.use_center)

    def circumpolar_today(self):
        """Return 'always_up' or 'never_up' when the body crosses the horizon
        neither way on the almanac's day, else None.

        PyEphem then raises AlwaysUpError or NeverUpError for that day's
        rise and set, even when the body crosses the next day, and WeeWX
        shows no time.
        """
        start = weeutil.weeutil.startOfDay(self.almanac.time_ts)
        today = calculate(observer_key(self.almanac), self.heavenly_body, 'events', start, start + 86400, self.use_center)
        if today['rises'] or today['sets']:
            return None
        return 'always_up' if today['always_up'] else 'never_up'

    @property
    def visible(self):
        events = self.event_data(weeutil.weeutil.startOfDay(self.almanac.time_ts))
        circumpolar = self.circumpolar_today()
        if circumpolar:
            value = 86400 if circumpolar == 'always_up' else 0
        elif events['rises'] and events['sets']:
            value = events['sets'][0] - events['rises'][0]
        else:
            value = 86400 if events['always_up'] else 0
        return helper(self.almanac, value, 'second', 'group_deltatime', 'day')

    def visible_change(self, days_ago=1):
        then = self.almanac(almanac_time=self.almanac.time_ts - days_ago * 86400)
        value = self.visible.value_t - Body(then, self.heavenly_body).visible.value_t
        return weewx.units.ValueHelper(value, context='hour', formatter=self.almanac.formatter,
                                       converter=self.almanac.converter)

    def __getattr__(self, attr):
        if attr.startswith('__'):
            raise AttributeError(attr)
        if attr in ['rise', 'set', 'transit'] or attr in [
                direction + '_' + event for direction in ['next', 'previous']
                for event in ['rising', 'setting', 'transit', 'antitransit']]:
            stamp = self.almanac.time_ts
            if attr in ['rise', 'set', 'transit']:
                stamp = weeutil.weeutil.startOfDay(stamp)
                if attr != 'transit' and self.circumpolar_today():
                    return helper(self.almanac, None, 'dublin_jd', 'group_time')
                attr = {'rise': 'next_rising', 'set': 'next_setting', 'transit': 'next_transit'}[attr]
            previous = attr.startswith('previous_')
            events = self.event_data(stamp, previous)
            key = {'rising': 'rises', 'setting': 'sets', 'transit': 'transits', 'antitransit': 'antitransits'}[attr.split('_', 1)[1]]
            values = [value for value in events[key] if value < stamp] if previous else [value for value in events[key] if value > stamp]
            value = (values[-1] if previous else values[0]) / 86400 + 25567.5 if values else None
            return helper(self.almanac, value, 'dublin_jd', 'group_time')
        fields = {'az': 'azimuth', 'alt': 'altitude', 'ra': 'right_ascension', 'dec': 'declination',
                  'g_ra': 'geocentric_ra', 'g_dec': 'geocentric_dec', 'radius': 'angular_radius'}
        aliases = {'azimuth': 'az', 'altitude': 'alt', 'topo_ra': 'ra', 'topo_dec': 'dec',
                   'geo_ra': 'g_ra', 'geo_dec': 'g_dec', 'radius_size': 'radius'}
        name = aliases.get(attr, attr)
        if name in fields or attr in ['earth_distance', 'phase', 'moon_fullness']:
            position = calculate(observer_key(self.almanac), self.heavenly_body, 'position')
            if attr in ['earth_distance', 'phase', 'moon_fullness']:
                return position[{'earth_distance': 'distance_au', 'phase': 'illumination', 'moon_fullness': 'moon_fullness'}[attr]]
            value = position[fields[name]]
            if attr not in aliases:
                return value
            if attr in ['azimuth', 'topo_ra', 'geo_ra']:
                return helper(self.almanac, value, 'degree_compass', 'group_direction')
            return helper(self.almanac, math.radians(value), 'radian', 'group_angle')
        # Optional PyEphem attributes outside the solar/lunar contract still
        # work when its original provider is installed.
        if 'ephem' in vars(weewx.almanac):
            return getattr(weewx.almanac.AlmanacBinder(self.almanac, self.heavenly_body), attr)
        raise AttributeError(attr)
