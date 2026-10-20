"""Source policies for the JSON generator shipped with WeeWX 5.6."""

import inspect
import logging
import os
import time

import weeutil.weeutil

from . import astronomy, native
from .batch import get_series_many

try:
    from weewx import jsongenerator as base
except ImportError as error:
    raise ImportError('weewxdb.json needs the JSON generator supplied with WeeWX 5.6') from error

log = logging.getLogger(__name__)


def _request(line, span, aggregate=None, interval=None, raw=False):
    options = dict(line['options'])
    options.pop('aggregate_type', None)
    options.pop('aggregate_interval', None)
    return {'observation': line['data_type'] if raw else line['var_type'], 'span': span,
            'aggregate': None if raw else aggregate or line['aggregate_type'],
            'interval': None if raw else interval or line['aggregate_interval'], 'options': options}


class JSONGenerator(base.JSONGenerator):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        expected = {'line', 'plot_options', 'mgr', 'timespan', 'aggregate_type',
                    'aggregate_interval', 'raw'}
        method = getattr(base.JSONGenerator, '_aggregate_line', None)
        self._supported = callable(method) and expected <= set(inspect.signature(method).parameters)
        if not self._supported:
            log.warning('Unrecognized WeeWX JSON generator interface; ignoring source policies')
            for name in ['_archive_series', '_aggregate_line', '_archive_daynight']:
                if hasattr(base.JSONGenerator, name):
                    setattr(self, name, getattr(super(), name))

    def _archive_series(self, line, *args, **kwargs):
        # The generator's empty-sensor check also has to use the requested
        # source. Otherwise a stored all-NULL column hides a derived series.
        token = native.report_source.set((line['data_type'], line['options'].get('source', 'auto')))
        try:
            return super()._archive_series(line, *args, **kwargs)
        finally:
            native.report_source.reset(token)

    def _aggregate_line(self, line, plot_options, mgr, timespan, aggregate_type=None,
                        aggregate_interval=None, raw=False):
        # A line without a source policy takes WeeWX's path, which reaches the
        # engine through the driver's XType. A policy needs the native request,
        # because it must fail rather than fall back to another source.
        if line['options'].get('source', 'auto') == 'auto':
            return super()._aggregate_line(line, plot_options, mgr, timespan, aggregate_type,
                                           aggregate_interval, raw)
        request = _request(line, timespan, aggregate_type, aggregate_interval, raw)
        result = get_series_many([request], mgr)[0]
        unit, values = self._convert(result[2], plot_options)
        return unit, list(zip(result[0][0], values))

    def _archive_daynight(self, root, first_ts, last_ts):
        if not first_ts or not last_ts:
            return
        for span in weeutil.weeutil.genYearSpans(first_ts, last_ts):
            path = os.path.join(root, 'daynight-%d.json' % time.localtime(span.start).tm_year)
            if os.path.exists(path) and span.stop <= last_ts:
                continue
            result = daynight(span.start, min(span.stop, int(last_ts)),
                              self.stn_info.latitude_f, self.stn_info.longitude_f, through=span.stop)
            if result is not None:
                result['start'] = int(span.start)
                base._write_json(path, result)


# The solar events of recent calls, keyed by (start, through, latitude, longitude).
_events = {}


def _solar_events(start, through, latitude, longitude):
    """Return the sun's horizon and civil twilight events from start to through.

    Returns:
        tuple[dict, dict]: A two-way tuple (sun, civil), each holding 'rises' and
            'sets' as timestamps, one day beyond either end.
    """
    key = (start, through, latitude, longitude)
    if key not in _events:
        if len(_events) >= 32:
            _events.clear()
        common = dict(operation='events', start=start - 86400, stop=through + 86400)
        _events[key] = (astronomy(start, latitude, longitude, **common),
                        astronomy(start, latitude, longitude, pressure=0, horizon=-6, center=True, **common))
    return _events[key]


def daynight(start, stop, latitude, longitude, through=None):
    """Return horizon crossings and civil twilight in the generator's JSON format.

    Args:
        start (int): The beginning of the timespan.
        stop (int): The end of the timespan.
        latitude (float): The station latitude, in degrees.
        longitude (float): The station longitude, in degrees.
        through (int|None): Where the events are calculated to, at or after 'stop'.
            The events of the year so far are then calculated once for the whole
            year and kept for the next report. Default is 'stop'.

    Returns:
        dict|None: As weewx.jsongenerator._daynight() returns it.
    """
    sun, civil = _solar_events(start, stop if through is None else through, latitude, longitude)
    transitions = sorted(int(t + 0.5) for t in sun['rises'] + sun['sets'] if start < t < stop)
    if not transitions:
        return None
    twilight = []
    for rises, bounds, direction in [(sun['rises'], civil['rises'], 'dawn'),
                                      (sun['sets'], civil['sets'], 'dusk')]:
        for crossing in rises:
            candidates = [t for t in bounds if 0 <= (crossing - t if direction == 'dawn' else t - crossing) < 86400]
            if not candidates:
                continue
            bound = min(candidates, key=lambda t: abs(t - crossing))
            begin, end = (bound, crossing) if direction == 'dawn' else (crossing, bound)
            if begin < stop and end > start:
                twilight.append({'from': int(begin + 0.5), 'to': int(end + 0.5), 'dir': direction})
    earliest = min((t, 'night') for t in sun['rises'] if start < t < stop) if any(start < t < stop for t in sun['rises']) else (float('inf'), 'night')
    sunset = min((t, 'day') for t in sun['sets'] if start < t < stop) if any(start < t < stop for t in sun['sets']) else (float('inf'), 'day')
    return {'first': min(earliest, sunset)[1], 'transitions': transitions,
            'twilight': sorted(twilight, key=lambda v: v['from'])}
