"""Batch adapter for the JSON generator shipped with WeeWX 5.6."""

import bisect
import inspect
import json
import logging
import os
import time

import weeutil.weeutil
import weewx
from weeutil.weeutil import TimeSpan

from . import astronomy, native
from .batch import ReportCache, get_series_many

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


def _key(manager, request):
    return id(manager), json.dumps(request, sort_keys=True, default=list)


class JSONGenerator(base.JSONGenerator):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        expected = {'plot_dict', 'plot_options', 'timespan', 'interval', 'rounding',
                    'group_name', 'first_ts', 'last_ts', 'old_file', 'extremes', 'raw'}
        archive_method = getattr(base.JSONGenerator, '_archive_file', None)
        self._batch_supported = callable(archive_method) and expected <= set(inspect.signature(archive_method).parameters) \
            and all(hasattr(base, name) for name in ['_extends', '_line_spec', '_intervals'])
        if not self._batch_supported:
            log.warning('Unrecognized WeeWX JSON generator interface; using ordinary XType dispatch')
            for name in ['_archive_file', '_archive_series', '_aggregate_line', '_archive_daynight']:
                if hasattr(base.JSONGenerator, name):
                    setattr(self, name, getattr(super(), name))
        self._prefetched = {}
        self._report_cache = ReportCache(int(float(self.skin_dict.get('JSONGenerator', {}).get('native_cache_mib', 8)) * 1024 * 1024))

    def _archive_file(self, plot_dict, plot_options, timespan, interval, rounding,
                      group_name, first_ts, last_ts, old_file=None, extremes=(), raw=False):
        if self._batch_supported:
            lo, hi = max(timespan.start, int(first_ts)), min(timespan.stop, int(last_ts) + interval)
            start, stop = int(lo // interval * interval), int(-(-hi // interval) * interval)
            count = (stop - start) // interval
            previous = old_file if old_file is not None and base._extends(old_file, start, interval, count) else None
            groups = {}
            if count >= 2:
                for name in plot_dict.sections:
                    line = base._line_spec(plot_dict[name], name, interval)
                    manager = self.db_binder.get_manager(line['options']['data_binding'])
                    if getattr(manager.connection, 'native', None) is None:
                        continue
                    if previous is None and line['data_type'] in manager.sqlkeys \
                            and line['options'].get('source', 'auto') in ['auto', 'stored'] \
                            and base._skip_if_empty(manager, line['data_type'], base._whole_days(start, stop)):
                        continue
                    def resume(seconds):
                        if previous is None:
                            return start
                        key = (start, stop, seconds, previous['newest'])
                        if key not in self._archive_resume:
                            begins, ends = base._intervals(start, stop, seconds)
                            i = bisect.bisect_left(ends, previous['newest'])
                            if len(self._archive_resume) >= 32:
                                self._archive_resume.clear()
                            self._archive_resume[key] = begins[i] if i < len(begins) else stop
                        return self._archive_resume[key]
                    unaggregated = raw and line['aggregate_interval'] == interval
                    requests = [_request(line, TimeSpan(resume(line['aggregate_interval']), stop), raw=unaggregated)]
                    if line['var_type'] in extremes and not unaggregated and not line['vector'] \
                            and line['aggregate_type'] not in ['min', 'max', 'vecdir']:
                        requests.extend(_request(line, TimeSpan(resume(interval), stop), which, interval)
                                        for which in ['min', 'max'])
                    for request in requests:
                        try:
                            native.prepare_series(request['observation'], request['span'], manager,
                                                  request['aggregate'], request['interval'], **request['options'])
                        except (weewx.UnknownType, weewx.UnknownAggregation):
                            continue
                        groups.setdefault(manager, {})[_key(manager, request)] = request
                for manager, requests in groups.items():
                    values = get_series_many(list(requests.values()), manager, self._report_cache)
                    self._prefetched.update(zip(requests, values))
        try:
            return super()._archive_file(plot_dict, plot_options, timespan, interval, rounding,
                                         group_name, first_ts, last_ts, old_file, extremes, raw)
        finally:
            self._prefetched.clear()

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
        request = _request(line, timespan, aggregate_type, aggregate_interval, raw)
        result = self._prefetched.get(_key(mgr, request))
        if result is None:
            if request['options'].get('source', 'auto') == 'auto':
                return super()._aggregate_line(line, plot_options, mgr, timespan, aggregate_type, aggregate_interval, raw)
            result = get_series_many([request], mgr, self._report_cache)[0]
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
                              self.stn_info.latitude_f, self.stn_info.longitude_f)
            if result is not None:
                result['start'] = int(span.start)
                base._write_json(path, result)


def daynight(start, stop, latitude, longitude):
    """Return horizon crossings and civil twilight in the generator's JSON format."""
    common = dict(operation='events', start=start - 86400, stop=stop + 86400)
    sun = astronomy(start, latitude, longitude, **common)
    civil = astronomy(start, latitude, longitude, pressure=0, horizon=-6, center=True, **common)
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
