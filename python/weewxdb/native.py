"""Keep WeeWX dispatch and calendar rules around native calculation requests."""

import contextvars
import contextlib
import datetime
import hashlib
import json
import math
import sys
import threading
import time
import weakref

import weeutil.weeutil
import weewx
import weewx.almanac
import weewx.units
import weewx.wxxtypes
import weewx.xtypes


WEATHER_INPUTS = {
    'dewpoint': ['outTemp', 'outHumidity'], 'inDewpoint': ['inTemp', 'inHumidity'],
    'humidex': ['outTemp', 'outHumidity'], 'heatindex': ['outTemp', 'outHumidity'],
    'windchill': ['outTemp', 'windSpeed'], 'appTemp': ['outTemp', 'outHumidity', 'windSpeed'],
    'cloudbase': ['outTemp', 'outHumidity'], 'beaufort': ['windSpeed'],
    'windrun': ['windSpeed', 'interval'], 'maxSolarRad': [],
    'windDir': ['windSpeed'], 'windGustDir': ['windGust'],
    'ET': ['interval'], 'pressure': ['outTemp', 'barometer', 'outHumidity'],
    'altimeter': ['pressure'], 'barometer': ['pressure', 'outTemp'], 'rainRate': [],
}
EXTRA_AGGREGATES = {'median', 'percentile', 'stddev', 'stddev_samp', 'variance',
                    'slope', 'rms', 'weighted_avg', 'integral', 'maxgap', 'longest_ge', 'longest_lt'}
HISTORICAL = {'historical_min', 'historical_max', 'historical_min_avg', 'historical_max_avg',
              'historical_mintime', 'historical_maxtime', 'historical_avg'}
report_source = contextvars.ContextVar('weewxdb_report_source', default=(None, 'auto'))
_provider_locks = weakref.WeakKeyDictionary()
_xaggs_sources = {}


def register_units():
    for name in ['historical_mintime', 'historical_maxtime', 'maxgap']:
        weewx.units.agg_group[name] = 'group_time' if name != 'maxgap' else 'group_deltatime'
    for name in ['avg_gt', 'avg_lt', 'longest_ge', 'longest_lt']:
        weewx.units.agg_group[name] = 'group_count'


def is_native(provider):
    from .xtypes import NativeXType
    return type(provider) is NativeXType


def scalar_provider(observation):
    for provider in weewx.xtypes.xtypes:
        if is_native(provider):
            continue
        method = type(provider).get_scalar
        if method is weewx.xtypes.XType.get_scalar:
            continue
        if 'get_scalar' in vars(provider) or getattr(method, '__module__', None) != 'weewx.wxxtypes':
            return None
        kind = type(provider)
        if kind is weewx.wxxtypes.WXXTypes:
            handles = observation in WEATHER_INPUTS and observation not in {
                'ET', 'pressure', 'altimeter', 'barometer', 'rainRate'}
        elif kind is weewx.wxxtypes.ETXType:
            handles = observation == 'ET'
        elif kind is weewx.wxxtypes.PressureCooker:
            handles = observation in ['pressure', 'altimeter', 'barometer']
        elif kind is weewx.wxxtypes.RainRater:
            handles = observation == 'rainRate'
        elif kind is weewx.wxxtypes.Delta:
            handles = observation in provider.totals
        else:
            return None
        if handles:
            if 'calc_%s' % observation in vars(provider):
                return None
            return provider
    return None


def provider_lock(provider):
    if type(provider) is weewx.wxxtypes.RainRater:
        return provider.run_lock
    if type(provider) in [weewx.wxxtypes.PressureCooker, weewx.wxxtypes.Delta]:
        if provider not in _provider_locks:
            _provider_locks[provider] = threading.RLock()
        return _provider_locks[provider]
    return contextlib.nullcontext()


def weather_specification(observation, manager, provider, units):
    parameters = {}
    inputs = WEATHER_INPUTS.get(observation, [])[:]
    formula = observation
    if observation == 'inDewpoint':
        formula = 'dewpoint'
    if hasattr(provider, 'altitude_vt'):
        parameters['altitude_m'] = weewx.units.convert(provider.altitude_vt, 'meter')[0]
    for name in ['latitude_f', 'longitude_f', 'atc', 'nfac', 'wind_height', 'et_period',
                 'albedo', 'cn', 'cd', 'max_delta_12h', 'rain_period', 'retain_period']:
        if hasattr(provider, name):
            parameters[{'latitude_f': 'latitude', 'longitude_f': 'longitude'}.get(name, name)] = getattr(provider, name)
    if observation == 'heatindex' and provider.heatindex_algo != 'new':
        formula = 'heatindex_old'
    if observation == 'maxSolarRad':
        from .almanac import NativeAlmanac
        if provider.maxSolarRad_algo not in ['rs', 'bras']:
            raise weewx.ViolatedPrecondition('Unknown solar algorithm %s' % provider.maxSolarRad_algo)
        if any(type(almanac) not in [NativeAlmanac, weewx.almanac.PyEphemAlmanacType, weewx.almanac.WeeutilAlmanacType]
               for almanac in weewx.almanac.almanacs):
            raise weewx.UnknownType(observation)
        formula = 'solar_' + provider.maxSolarRad_algo
        parameters['solar_enabled'] = 1
    if observation in ['windDir', 'windGustDir']:
        parameters['force_null'] = int(provider.force_null)
    if type(provider) is weewx.wxxtypes.PressureCooker:
        parameters['altimeter_algorithm'] = provider.altimeter_algorithm
        parameters['lookup_time'] = provider.ts_12h
        parameters['temperature_present'] = provider.temp_12h_vt is not None
        parameters['temperature_f'] = (weewx.units.convert(provider.temp_12h_vt, 'degree_F')[0]
                                         if provider.temp_12h_vt is not None else None)
    if type(provider) is weewx.wxxtypes.RainRater:
        parameters.update(rain_events=provider.rain_events, rain_units=provider.unit_system,
                          augmented=provider.augmented)
    if type(provider) is weewx.wxxtypes.Delta:
        formula = 'delta'
        inputs = [provider.totals[observation][0]]
        parameters['previous'] = provider.totals[observation][1]
    return {'formula': formula, 'inputs': inputs, 'unit_system': units, 'parameters': parameters}


def apply_state(provider, observation, state, manager):
    if type(provider) is weewx.wxxtypes.Delta:
        provider.totals[observation][1] = state['previous']
    elif type(provider) is weewx.wxxtypes.RainRater:
        provider.rain_events = [tuple(event) for event in state['rain_events']]
        provider.unit_system = state['rain_units']
        provider.augmented = state['augmented']
    elif type(provider) is weewx.wxxtypes.PressureCooker:
        provider.ts_12h = state['lookup_time']
        if state.get('temperature_present', state['temperature_f'] is not None):
            value = weewx.units.ValueTuple(state['temperature_f'], 'degree_F', 'group_temperature')
            provider.temp_12h_vt = weewx.units.convertStd(value, manager.std_unit_system)
        else:
            provider.temp_12h_vt = None


def request(manager, query):
    query.setdefault('table', manager.table_name)
    query.setdefault('unit_system', manager.std_unit_system)
    try:
        return manager.connection.native.analytics_native(query)
    except ValueError as error:
        if 'integer precision requires SQL' in str(error):
            raise weewx.UnknownType(query['observation']) from error
        if 'unit system changes' in str(error):
            raise weewx.UnsupportedFeature(str(error)) from error
        if 'no calculate' in str(error):
            raise weewx.NoCalculate from error
        raise


def scalar(observation, record, manager):
    if manager is None or getattr(manager.connection, 'native', None) is None:
        raise weewx.UnknownType(observation)
    provider = scalar_provider(observation)
    if provider is None:
        raise weewx.UnknownType(observation)
    with provider_lock(provider):
        spec = weather_specification(observation, manager, provider, record['usUnits'])
        if observation == 'rainRate':
            spec['parameters']['event_time'] = record['dateTime']
        if any(name not in record for name in spec['inputs']):
            if observation in ['windDir', 'windGustDir']:
                raise weewx.NoCalculate
            raise weewx.CannotCalculate(observation)
        result = request(manager, {'mode': 'weather', 'observation': observation, 'aggregate': 'avg',
                                   'ranges': [[math.floor(record['dateTime']), math.floor(record['dateTime'])]],
                                   'weather': spec, 'scalar_args': [record[name] for name in spec['inputs']]})
        apply_state(provider, observation, result['state'], manager)
        unit, group = weewx.units.getStandardUnitType(record['usUnits'], observation)
        if observation == 'pressure':
            unit = 'inHg'
        elif observation == 'beaufort':
            unit, group = None, None
        elif observation == 'windrun' and record['windSpeed'] is None:
            unit = 'mile'
        elif observation == 'ET':
            unit, group = ('inch', 'group_rain') if result['state']['et_available'] else (None, None)
        elif observation == 'rainRate':
            unit, group = weewx.units.getStandardUnitType(provider.unit_system, observation)
        return weewx.units.ValueTuple(result['values'][0], unit, group)


def native_xaggs(provider):
    kind = type(provider)
    if kind.__name__ not in ['XAggsHistorical', 'XAggsAvg']:
        return False
    module = sys.modules.get(kind.__module__)
    path = getattr(module, '__file__', None)
    if path not in _xaggs_sources:
        try:
            with open(path, 'rb') as source:
                _xaggs_sources[path] = hashlib.sha256(source.read()).hexdigest() == '627b724da2b617c1777cf09c6b680e4c722ddc66ed9ea8169f262e8831936737'
        except (OSError, TypeError):
            _xaggs_sources[path] = False
    return _xaggs_sources[path] and 'get_aggregate' not in vars(provider)


def dispatch_allowed(method, target):
    for provider in weewx.xtypes.xtypes:
        if is_native(provider):
            continue
        if type(provider) is target:
            return method not in vars(provider)
        implementation = getattr(type(provider), method)
        if implementation is getattr(weewx.xtypes.XType, method):
            continue
        if implementation.__module__ == 'weewx.xtypes' and method not in vars(provider):
            continue
        if native_xaggs(provider):
            continue
        return False
    return True


def eligible(observation, span, manager, aggregate):
    try:
        weewx.xtypes.DailySummaries.check_eligibility(observation, span, manager, aggregate)
        return True
    except (weewx.UnknownType, weewx.UnknownAggregation):
        return False


def threshold_value(options, manager):
    value = options.get('val')
    if value is None:
        return None
    if len(value) == 2:
        value = tuple(value) + ({'degree_F': 'group_temperature', 'degree_C': 'group_temperature',
                                 'inch': 'group_rain', 'mm': 'group_rain', 'cm': 'group_rain'}[value[1]],)
    return weewx.units.convertStd((float(value[0]), value[1], value[2]), manager.std_unit_system)[0]


def query_for(observation, span, aggregate, manager, options):
    if getattr(manager.connection, 'native', None) is None:
        raise weewx.UnknownType(observation)
    start, stop = map(math.floor, span)
    query = {'mode': 'archive', 'observation': observation, 'aggregate': aggregate,
             'ranges': [[start, stop]], 'threshold': None}
    source = options.get('source', 'auto')
    if source not in ['auto', 'stored', 'derived', 'fill_missing']:
        raise ValueError('Unknown source policy: %s' % source)
    if source != 'auto':
        if aggregate not in ['avg', 'min', 'max', 'sum', 'count', 'not_null',
                              'mintime', 'maxtime', 'first', 'last', 'firsttime',
                              'lasttime', 'cumulative', 'weighted_avg', 'integral']:
            raise weewx.UnknownAggregation(aggregate)
        query['source'] = source
        if source == 'stored':
            if observation not in manager.sqlkeys or not dispatch_allowed('get_aggregate', weewx.xtypes.ArchiveTable):
                raise weewx.UnknownType(observation)
        else:
            provider = scalar_provider(observation)
            if provider is None or not dispatch_allowed('get_aggregate', weewx.xtypes.XTypeTable):
                raise weewx.UnknownType(observation)
            spec = weather_specification(observation, manager, provider, manager.std_unit_system)
            if any(name not in manager.sqlkeys for name in spec['inputs']):
                raise weewx.UnknownType(observation)
            query.update(mode='weather', weather=spec)
        return query
    if aggregate in HISTORICAL:
        if not dispatch_allowed('get_aggregate', None):
            raise weewx.UnknownAggregation(aggregate)
        begin = datetime.date.fromtimestamp(start)
        end = datetime.date.fromtimestamp(stop)
        if manager.first_timestamp is None or manager.last_timestamp is None \
                or not (weeutil.weeutil.isStartOfDay(start) or start == manager.first_timestamp) \
                or not (weeutil.weeutil.isStartOfDay(stop) or stop == manager.last_timestamp) \
                or (end - begin).days != 1:
            raise weewx.UnknownAggregation(aggregate)
        query.update(mode='historical', table='%s_day_%s' % (manager.table_name, observation),
                     calendar_days=[(begin.month, begin.day)])
    elif observation in ['heatdeg', 'cooldeg', 'growdeg']:
        if aggregate not in ['sum', 'avg', 'not_null'] or not dispatch_allowed('get_aggregate', weewx.xtypes.AggregateHeatCool):
            raise weewx.UnknownAggregation(aggregate)
        base_key = {'heatdeg': 'heating_base', 'cooldeg': 'cooling_base', 'growdeg': 'growing_base'}[observation]
        default = weewx.xtypes.AggregateHeatCool.default_growbase if observation == 'growdeg' else weewx.xtypes.AggregateHeatCool.default_heatbase
        base = options.get('skin_dict', {}).get('Units', {}).get('DegreeDays', {}).get(base_key, default)
        query.update(mode='degree', table='%s_day_outTemp' % manager.table_name,
                     threshold=weewx.units.convertStd((float(base[0]), base[1], 'group_temperature'), manager.std_unit_system)[0])
        days = list(weeutil.weeutil.genDaySpans(start, stop))
        query['ranges'] = [[days[0].start, days[-1].stop]] if days else []
    elif observation in ['windvec', 'windgustvec']:
        if not dispatch_allowed('get_aggregate', weewx.xtypes.WindVecDaily):
            raise weewx.UnknownAggregation(aggregate)
        if observation == 'windvec' and aggregate in ['avg', 'not_null'] and eligible('wind', span, manager, aggregate):
            query.update(mode='daily', observation='wind', aggregate='windvec_avg' if aggregate == 'avg' else aggregate,
                         table='%s_day_wind' % manager.table_name,
                         ranges=[[weeutil.weeutil.startOfDay(start), stop]])
        elif aggregate in ['sum', 'avg', 'min', 'max', 'first', 'last', 'count', 'not_null'] or aggregate is None:
            query.update(mode='vector')
        else:
            raise weewx.UnknownAggregation(aggregate)
    elif aggregate in weewx.xtypes.DailySummaries.agg_sql_dict and eligible(observation, span, manager, aggregate):
        if not dispatch_allowed('get_aggregate', weewx.xtypes.DailySummaries):
            raise weewx.UnknownAggregation(aggregate)
        query.update(mode='daily', table='%s_day_%s' % (manager.table_name, observation),
                     ranges=[[weeutil.weeutil.startOfDay(start), stop]], threshold=threshold_value(options, manager))
    elif aggregate in ['avg_ge', 'avg_gt', 'avg_le', 'avg_lt']:
        if not dispatch_allowed('get_aggregate', None):
            raise weewx.UnknownAggregation(aggregate)
        query.update(mode='xagg_avg', table='%s_day_%s' % (manager.table_name, observation),
                     threshold=threshold_value(options, manager))
    elif aggregate in ['vecavg', 'vecdir', 'gustdir']:
        if not dispatch_allowed('get_aggregate', weewx.xtypes.ArchiveTable):
            raise weewx.UnknownAggregation(aggregate)
        query.update(mode='gustdir' if aggregate == 'gustdir' else 'wind')
    elif observation == 'wind':
        if not dispatch_allowed('get_aggregate', weewx.xtypes.ArchiveTable):
            raise weewx.UnknownAggregation(aggregate)
        query['observation'] = 'windGust' if aggregate in ['max', 'maxtime'] else 'windSpeed'
    elif observation not in manager.sqlkeys:
        provider = scalar_provider(observation)
        if provider is None or not dispatch_allowed('get_aggregate', weewx.xtypes.XTypeTable):
            raise weewx.UnknownType(observation)
        spec = weather_specification(observation, manager, provider, manager.std_unit_system)
        if any(name not in manager.sqlkeys for name in spec['inputs']):
            raise weewx.UnknownType(observation)
        query.update(mode='weather', weather=spec)
    elif not dispatch_allowed('get_aggregate', weewx.xtypes.ArchiveTable):
        raise weewx.UnknownType(observation)
    if aggregate in EXTRA_AGGREGATES:
        if aggregate == 'percentile':
            query['threshold'] = float(options.get('fraction', 0.5))
        elif aggregate.startswith('longest_'):
            query['threshold'] = threshold_value(options, manager)
            if eligible(observation, span, manager, 'avg'):
                query.update(mode='daily', table='%s_day_%s' % (manager.table_name, observation))
    return query


def aggregate(observation, span, aggregate_type, manager, **options):
    aggregate_type = aggregate_type.lower()
    # Keep the existing two-column kernels, including their no-record-loading
    # contract, for the previously implemented five simple reductions.
    if observation in ['dewpoint', 'inDewpoint', 'humidex', 'heatindex'] \
            and options.get('source', 'auto') == 'auto' and observation not in manager.sqlkeys and aggregate_type in ['sum', 'avg', 'min', 'max', 'count']:
        raise weewx.UnknownType(observation)
    supported = set(weewx.xtypes.ArchiveTable.valid_aggregate_types) \
        | set(weewx.xtypes.DailySummaries.agg_sql_dict) | HISTORICAL | EXTRA_AGGREGATES \
        | {'avg_gt', 'avg_lt', 'not_null'}
    if aggregate_type not in supported:
        raise weewx.UnknownAggregation(aggregate_type)
    provider = scalar_provider(observation) if observation not in manager.sqlkeys or options.get('source') in ['derived', 'fill_missing'] else None
    with provider_lock(provider) if provider is not None else contextlib.nullcontext():
        query = query_for(observation, span, aggregate_type, manager, options)
        result = request(manager, query)
        if provider is not None and query['mode'] == 'weather':
            apply_state(provider, observation, result['state'], manager)
    return finish_aggregate(observation, aggregate_type, manager, query, result)


def finish_aggregate(observation, aggregate_type, manager, query, result):
    value = result['values'][0] if result['values'] else (0 if aggregate_type == 'sum' else None)
    if isinstance(value, list):
        value = complex(*value)
    units = manager.std_unit_system
    if query['mode'] in ['weather', 'vector']:
        units = result['units'][0] if result['units'] else None
    unit, group = weewx.units.getStandardUnitType(units, observation, aggregate_type)
    if aggregate_type in ['variance', 'slope', 'integral'] \
            or aggregate_type in ['stddev', 'stddev_samp'] and group == 'group_temperature':
        # WeeWX has no units for these dimensions. An ordinary temperature
        # group would apply an absolute C/F offset to a standard deviation.
        unit, group = None, None
    if aggregate_type == 'tderiv':
        if unit in ['watt_hour', 'kilowatt_hour'] and value is not None:
            value *= 3600
        unit = {'watt_second': 'watt', 'watt_hour': 'watt', 'kilowatt_hour': 'kilowatt'}.get(unit, unit)
        group = 'group_power'
    return weewx.units.ValueTuple(value, unit, group)


def prepare_series(observation, span, manager, aggregate_type=None, aggregate_interval=None, **options):
    """Plan a series using WeeWX calendar boundaries and dispatcher priority."""
    if getattr(manager.connection, 'native', None) is None:
        raise weewx.UnknownType(observation)
    interval = weeutil.weeutil.nominal_spans(aggregate_interval) if aggregate_type else None
    use_daily = options.get('source', 'auto') == 'auto' \
        and aggregate_type in weewx.xtypes.DailySummaries.common \
        and eligible(observation, span, manager, aggregate_type) \
        and interval is not None and (interval % 86400 == 0 or interval in [
            weeutil.weeutil.nominal_intervals['month'], weeutil.weeutil.nominal_intervals['year']])
    if not dispatch_allowed('get_series', weewx.xtypes.ArchiveTable):
        raise weewx.UnknownType(observation)
    start, stop = map(math.floor, span)
    ranges = []
    if aggregate_type is None:
        query = query_for(observation, span, 'avg', manager, options)
        query.update(raw=True)
    else:
        if aggregate_interval is None:
            raise weewx.UnknownAggregation(aggregate_interval)
        if use_daily:
            if interval == weeutil.weeutil.nominal_intervals['month']:
                spans = weeutil.weeutil.genMonthSpans(start, stop)
            elif interval == weeutil.weeutil.nominal_intervals['year']:
                spans = weeutil.weeutil.genYearSpans(start, stop)
            else:
                spans = weeutil.weeutil.intervalgen(weeutil.weeutil.startOfDay(start), stop, interval)
            ranges = [(max(math.floor(s.start), start), min(math.floor(s.stop), stop)) for s in spans]
            query = query_for(observation, span, aggregate_type, manager, options)
            query['ranges'] = ranges
        else:
            ranges = [(math.floor(s.start), math.floor(s.stop))
                      for s in weeutil.weeutil.intervalgen(start, stop, aggregate_interval)
                      if manager.first_timestamp is not None and s.stop > manager.first_timestamp
                      and manager.last_timestamp is not None and s.start < manager.last_timestamp]
            if not ranges:
                query = {'mode': 'archive', 'observation': observation, 'aggregate': aggregate_type, 'ranges': []}
            else:
                query = query_for(observation, ranges[0], 'sum' if aggregate_type == 'cumulative' else aggregate_type, manager, options)
                query['ranges'] = ranges
                if query['mode'] == 'historical':
                    for selected in ranges:
                        query_for(observation, selected, aggregate_type, manager, options)
                    query['calendar_days'] = [(datetime.date.fromtimestamp(a).month, datetime.date.fromtimestamp(a).day) for a, b in ranges]
                elif query['mode'] == 'degree':
                    expanded = [list(weeutil.weeutil.genDaySpans(a, b)) for a, b in ranges]
                    query['ranges'] = [(days[0].start, days[-1].stop) for days in expanded]
                elif query['mode'] == 'daily':
                    query['ranges'] = [(weeutil.weeutil.startOfDay(a), b) for a, b in ranges]
                if aggregate_type == 'cumulative':
                    query['aggregate'] = 'cumulative'
    return query, ranges, use_daily


def finish_series(observation, aggregate_type, manager, plan, result):
    query, ranges, use_daily = plan
    starts, stops = ([r[0] for r in ranges], [r[1] for r in ranges]) if aggregate_type else (result['starts'], result['stops'])
    values = result['values']
    if use_daily:
        occupied = [i for i, count in enumerate(result['rows']) if count]
        starts = [result['starts'][i] for i in occupied]
        stops = [int(time.mktime((datetime.date.fromtimestamp(result['stops'][i]) + datetime.timedelta(days=1)).timetuple())) for i in occupied]
        values = [values[i] for i in occupied]
    values = [complex(*v) if isinstance(v, list) else v for v in values]
    units = manager.std_unit_system if ranges or aggregate_type is None else None
    if query['mode'] in ['weather', 'vector']:
        units = next((unit for unit in result['units'] if unit is not None), None)
    unit, group = weewx.units.getStandardUnitType(units, observation, aggregate_type)
    if aggregate_type in ['variance', 'slope', 'integral'] \
            or aggregate_type in ['stddev', 'stddev_samp'] and group == 'group_temperature':
        unit, group = None, None
    return (weewx.units.ValueTuple(starts, 'unix_epoch', 'group_time'),
            weewx.units.ValueTuple(stops, 'unix_epoch', 'group_time'),
            weewx.units.ValueTuple(values, unit, group))


def series(observation, span, manager, aggregate_type=None, aggregate_interval=None, **options):
    provider = scalar_provider(observation) if observation not in manager.sqlkeys or options.get('source') in ['derived', 'fill_missing'] else None
    with provider_lock(provider) if provider is not None else contextlib.nullcontext():
        plan = prepare_series(observation, span, manager, aggregate_type, aggregate_interval, **options)
        old_weather = observation in ['dewpoint', 'inDewpoint', 'humidex', 'heatindex']
        old_aggregate = aggregate_type in [None, 'sum', 'avg', 'count', 'min', 'max', 'cumulative']
        if options.get('source', 'auto') == 'auto' and old_aggregate \
                and (old_weather or observation in manager.sqlkeys) and not plan[2]:
            raise weewx.UnknownType(observation)
        result = request(manager, plan[0])
        if provider is not None and plan[0]['mode'] == 'weather':
            apply_state(provider, observation, result['state'], manager)
    return finish_series(observation, aggregate_type, manager, plan, result)
