"""Optional batch APIs for reports; ordinary WeeWX dispatch remains available."""

import collections
import contextlib
import json
import sys

import weewx
import weewx.units
import weewx.xtypes

from . import native


class ReportCache:
    """Bound cached Python results by their measured object sizes."""

    def __init__(self, max_bytes=8 * 1024 * 1024):
        self.max_bytes = max(0, int(max_bytes))
        self.bytes = 0
        self.entries = collections.OrderedDict()
        self.revision = None

    def refresh(self, connection):
        revision = (id(connection), connection.revision())
        if revision[1] is None or self.revision != revision:
            self.entries.clear()
            self.bytes = 0
            self.revision = revision
        return revision[1] is not None

    def get(self, key):
        entry = self.entries.get(key)
        if entry is None:
            return None
        self.entries.move_to_end(key)
        return _clone(entry[0])

    def put(self, key, value):
        value = _clone(value)
        size = _size(value) + _size(key)
        if size > self.max_bytes:
            return
        if key in self.entries:
            self.bytes -= self.entries.pop(key)[1]
        while self.entries and self.bytes + size > self.max_bytes:
            self.bytes -= self.entries.popitem(last=False)[1][1]
        self.entries[key] = value, size
        self.bytes += size


def _clone(value):
    # ValueTuple.__new__ differs between WeeWX versions; deepcopy can nest the
    # entire tuple in its value field. Reconstruct its three documented fields.
    if isinstance(value, weewx.units.ValueTuple):
        return weewx.units.ValueTuple(_clone(value[0]), value[1], value[2])
    if isinstance(value, list):
        return [_clone(item) for item in value]
    if isinstance(value, tuple):
        return tuple(_clone(item) for item in value)
    if isinstance(value, dict):
        return {key: _clone(item) for key, item in value.items()}
    return value


def _size(value):
    size = sys.getsizeof(value)
    if isinstance(value, (list, tuple)):
        size += sum(_size(item) for item in value)
    elif isinstance(value, dict):
        size += sum(_size(key) + _size(item) for key, item in value.items())
    return size


def _provider(observation, manager, options):
    return native.scalar_provider(observation) if observation not in manager.sqlkeys \
        or options.get('source') in ['derived', 'fill_missing'] else None


def get_series_many(requests, manager, cache=None, statistics=None):
    """Return series in request order, falling back to WeeWX for custom XTypes.

    Each request is a dictionary with observation, span, aggregate (or None),
    interval (or None), and optional options. Explicit source policies raise an
    error if unsupported; they never silently return values from another source.
    """
    results = [None] * len(requests)
    connection = getattr(manager.connection, 'native', None)
    pending = []
    with contextlib.ExitStack() as stack:
        providers = {_provider(r['observation'], manager, r.get('options', {})) for r in requests}
        for provider in sorted((p for p in providers if p is not None), key=id):
            stack.enter_context(native.provider_lock(provider))
        cached = cache is not None and connection is not None and cache.refresh(connection)
        for i, request in enumerate(requests):
            obs = request['observation']
            options = request.get('options', {})
            try:
                plan = native.prepare_series(obs, request['span'], manager,
                                             request.get('aggregate'), request.get('interval'), **options)
            except (weewx.UnknownType, weewx.UnknownAggregation):
                if options.get('source', 'auto') != 'auto':
                    raise
                results[i] = weewx.xtypes.get_series(obs, request['span'], manager,
                                                   request.get('aggregate'), request.get('interval'), **options)
                continue
            query = plan[0]
            query.update(table=query.get('table', manager.table_name), unit_system=manager.std_unit_system)
            stateful = query.get('weather', {}).get('formula') in ['delta', 'rainRate', 'pressure']
            key = json.dumps(query, sort_keys=True, separators=(',', ':')) if cached and not stateful else None
            hit = cache.get(key) if key is not None else None
            if hit is not None:
                results[i] = hit
            else:
                pending.append((i, plan, key))
        if pending:
            try:
                answer = connection.analytics_many_native([plan[0] for _, plan, _ in pending])
            except ValueError as error:
                # An exact integer or a large batch may need the established
                # per-query path. Policies remain explicit on these retries.
                if not any(reason in str(error) for reason in ['workspace', 'integer precision requires SQL']):
                    raise
                answer = {'answers': [], 'scans': 0, 'formula_evaluations': 0}
                for i, plan, key in pending:
                    request = requests[i]
                    if request.get('options', {}).get('source', 'auto') != 'auto':
                        answer['answers'].append(native.request(manager, plan[0]))
                    else:
                        results[i] = weewx.xtypes.get_series(request['observation'], request['span'], manager,
                                                           request.get('aggregate'), request.get('interval'), **request.get('options', {}))
                        answer['answers'].append(None)
            for (i, plan, key), result in zip(pending, answer['answers']):
                if result is None:
                    continue
                request = requests[i]
                obs = request['observation']
                provider = _provider(obs, manager, request.get('options', {}))
                if provider is not None and plan[0]['mode'] == 'weather' \
                        and (type(provider).__name__ != 'PressureCooker' or obs == 'pressure'):
                    native.apply_state(provider, obs, result['state'], manager)
                results[i] = native.finish_series(obs, request.get('aggregate'), manager, plan, result)
                if key is not None:
                    cache.put(key, results[i])
            if statistics is not None:
                statistics.update(scans=answer['scans'], formula_evaluations=answer['formula_evaluations'])
    return results


def get_aggregates_many(observation, span, aggregates, manager, **options):
    """Return ValueTuples in aggregate order, preserving daily summary semantics."""
    connection = getattr(manager.connection, 'native', None)
    provider = _provider(observation, manager, options)
    with native.provider_lock(provider) if provider is not None else contextlib.nullcontext():
        queries = []
        indices = []
        results = [None] * len(aggregates)
        for i, aggregate in enumerate(aggregates):
            try:
                query = native.query_for(observation, span, aggregate, manager, options)
            except (weewx.UnknownType, weewx.UnknownAggregation):
                if options.get('source', 'auto') != 'auto':
                    raise
                results[i] = weewx.xtypes.get_aggregate(observation, span, aggregate, manager, **options)
            else:
                query.setdefault('table', manager.table_name)
                query.setdefault('unit_system', manager.std_unit_system)
                queries.append(query)
                indices.append(i)
        if queries:
            try:
                answers = connection.analytics_many_native(queries)['answers']
            except ValueError as error:
                if 'integer precision requires SQL' not in str(error) and 'workspace' not in str(error):
                    raise
                for i in indices:
                    results[i] = weewx.xtypes.get_aggregate(observation, span, aggregates[i], manager, **options)
            else:
                for i, query, answer in zip(indices, queries, answers):
                    results[i] = native.finish_aggregate(observation, aggregates[i], manager, query, answer)
                    if provider is not None and query['mode'] == 'weather' \
                            and (type(provider).__name__ != 'PressureCooker' or observation == 'pressure'):
                        native.apply_state(provider, observation, answer['state'], manager)
        return results
