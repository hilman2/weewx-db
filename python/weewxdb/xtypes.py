"""Send archive series to Rust while keeping WeeWX's daily-summary rules."""

import math

import weeutil.weeutil
import weewx
import weewx.units
import weewx.xtypes
import weewx.wxxtypes

from . import native as calculations


def computed_specification(obs_type, db_manager):
    definitions = {'dewpoint': ('dewpoint', ('outTemp', 'outHumidity')),
                   'inDewpoint': ('dewpoint', ('inTemp', 'inHumidity')),
                   'humidex': ('humidex', ('outTemp', 'outHumidity')),
                   'heatindex': ('heatindex', ('outTemp', 'outHumidity'))}
    if obs_type not in definitions or db_manager.std_unit_system not in [1, 16, 17]:
        return None
    formula, inputs = definitions[obs_type]
    if not all(name in db_manager.sqlkeys for name in inputs):
        return None
    if obs_type in getattr(db_manager, 'daykeys', []):
        return None
    for provider in weewx.xtypes.xtypes:
        if type(provider) is NativeXType:
            continue
        if any(name in vars(provider) for name in ['get_series', 'get_aggregate', 'get_scalar']):
            return None
        if any(getattr(getattr(type(provider), name), '__module__', None) != weewx.xtypes.__name__
               for name in ['get_series', 'get_aggregate']):
            return None
        if type(provider).get_scalar is weewx.xtypes.XType.get_scalar:
            continue
        # A preceding custom scalar provider can change the meaning of the
        # observation. Keep WeeWX's dispatch order rather than guessing which
        # names such a provider will handle.
        if type(provider) is not weewx.wxxtypes.WXXTypes:
            return None
        if 'calc_%s' % obs_type in vars(provider):
            return None
        if formula == 'heatindex' and provider.heatindex_algo != 'new':
            formula = 'heatindex_old'
        return formula, inputs, db_manager.std_unit_system
    return None


def computed_call(function, *args):
    try:
        return function(*args)
    except ValueError as error:
        if 'unit system changes' in str(error):
            raise weewx.UnsupportedFeature(str(error)) from error
        raise


def register():
    from . import almanac
    calculations.register_units()
    almanac.register()
    if not any(isinstance(xtype, NativeXType) for xtype in weewx.xtypes.xtypes):
        weewx.xtypes.xtypes.insert(0, NativeXType())


class NativeXType(weewx.xtypes.XType):
    def get_scalar(self, obs_type, record, db_manager=None, **options):
        return calculations.scalar(obs_type, record, db_manager)

    def get_series(self, obs_type, timespan, db_manager, aggregate_type=None,
                   aggregate_interval=None, **options):
        try:
            return calculations.series(obs_type, timespan, db_manager, aggregate_type,
                                       aggregate_interval, **options)
        except (weewx.UnknownType, weewx.UnknownAggregation) as error:
            if options.get('source', 'auto') != 'auto':
                raise weewx.UnsupportedFeature('Unsupported explicit source policy for %s' % obs_type) from error
            pass
        if not calculations.dispatch_allowed('get_series', weewx.xtypes.ArchiveTable) \
                or aggregate_type is not None and not calculations.dispatch_allowed('get_aggregate', weewx.xtypes.ArchiveTable):
            raise weewx.UnknownType(obs_type)
        start, stop = timespan
        native = getattr(db_manager.connection, 'native', None)
        if native is None:
            raise weewx.UnknownType(obs_type)
        computed = None
        if obs_type not in db_manager.sqlkeys:
            computed = computed_specification(obs_type, db_manager)
            if computed is None:
                raise weewx.UnknownType(obs_type)
        if aggregate_type is None:
            if computed is None:
                starts, stops, values = native.raw_series(
                    db_manager.table_name, obs_type, math.floor(start), math.floor(stop))
            else:
                starts, stops, values = computed_call(
                    native.computed_raw, db_manager.table_name, computed,
                    math.floor(start), math.floor(stop))
            unit_system = db_manager.std_unit_system if computed is None or values else None
            unit, group = weewx.units.getStandardUnitType(unit_system, obs_type)
            return (weewx.units.ValueTuple(starts, 'unix_epoch', 'group_time'),
                    weewx.units.ValueTuple(stops, 'unix_epoch', 'group_time'),
                    weewx.units.ValueTuple(values, unit, group))
        if aggregate_type not in ['sum', 'avg', 'count', 'min', 'max', 'cumulative']:
            raise weewx.UnknownAggregation(aggregate_type)
        if aggregate_interval is None:
            raise weewx.UnknownAggregation(aggregate_interval)
        interval = weeutil.weeutil.nominal_spans(aggregate_interval)
        ranges = [(a, b) for a, b in calculations.intervals(start, stop, aggregate_interval)
                  if db_manager.first_timestamp is not None
                  and b > db_manager.first_timestamp
                  and db_manager.last_timestamp is not None
                  and a < db_manager.last_timestamp]
        if computed is None and calculations.daily_bins(obs_type, ranges, db_manager, aggregate_type, interval):
            # Daily extrema may originate in LOOP packets. They cannot be
            # replaced by extrema calculated only from archive records, so
            # WeeWX aggregates each bin through get_aggregate.
            raise weewx.UnknownAggregation(aggregate_type)
        if computed is None:
            values = native.series(db_manager.table_name, obs_type, ranges, aggregate_type)
            unit_system = db_manager.std_unit_system
        else:
            values, source_rows = computed_call(native.computed_series, db_manager.table_name,
                                                computed, ranges, aggregate_type)
            unit_system = db_manager.std_unit_system if source_rows else None
        unit, group = weewx.units.getStandardUnitType(
            unit_system, obs_type, aggregate_type)
        return (weewx.units.ValueTuple([span[0] for span in ranges], 'unix_epoch', 'group_time'),
                weewx.units.ValueTuple([span[1] for span in ranges], 'unix_epoch', 'group_time'),
                weewx.units.ValueTuple(values, unit, group))

    def get_aggregate(self, obs_type, timespan, aggregate_type, db_manager, **options):
        observation, source = calculations.report_source.get()
        if obs_type == observation and source != 'auto':
            options = dict(options, source=source)
        try:
            return calculations.aggregate(obs_type, timespan, aggregate_type, db_manager, **options)
        except (weewx.UnknownType, weewx.UnknownAggregation) as error:
            if options.get('source', 'auto') != 'auto':
                raise weewx.UnsupportedFeature('Unsupported explicit source policy for %s' % obs_type) from error
            pass
        native = getattr(db_manager.connection, 'native', None)
        if native is None or obs_type in db_manager.sqlkeys:
            raise weewx.UnknownType(obs_type)
        computed = computed_specification(obs_type, db_manager)
        if computed is None:
            raise weewx.UnknownType(obs_type)
        if aggregate_type not in ['sum', 'avg', 'min', 'max', 'count']:
            raise weewx.UnknownAggregation(aggregate_type)
        start, stop = timespan
        rows, count, total, minimum, maximum = computed_call(
            native.computed_aggregate, db_manager.table_name, computed,
            math.floor(start), math.floor(stop))
        value = {'count': count, 'sum': total if total is not None else 0.0,
                 'min': minimum, 'max': maximum,
                 'avg': total / count if count else None}[aggregate_type]
        unit, group = weewx.units.getStandardUnitType(
            db_manager.std_unit_system if rows else None, obs_type, aggregate_type)
        return weewx.units.ValueTuple(value, unit, group)
