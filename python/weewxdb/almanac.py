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

    @property
    def visible(self):
        events = self.event_data(weeutil.weeutil.startOfDay(self.almanac.time_ts))
        if events['rises'] and events['sets']:
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
