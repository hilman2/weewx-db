"""Optional service replacement that calculates ordered packet fields in Rust."""

import contextlib
import logging

import weedb
import weewx.engine
import weewx.wxservices

from . import native

log = logging.getLogger(__name__)


class StdWXCalculate(weewx.wxservices.StdWXCalculate):
    def do_calculations(self, data_dict, calc_dict=None):
        directives = self.archive_calc_dict if calc_dict is None else calc_dict
        if getattr(self.db_manager.connection, 'native', None) is None:
            return super().do_calculations(data_dict, directives)
        pending = []

        def flush():
            if not pending:
                return
            batch = list(pending)
            pending.clear()
            try:
                calculate(batch)
            except weedb.DatabaseError:
                raise
            except Exception as error:
                # The native step changes the packet only after it succeeded.
                # WeeWX's own calculation answers instead, as without this
                # service; a packet value it cannot convert, such as NaN,
                # ends up here.
                log.debug('Native packet calculation failed (%s); using WeeWX for %s',
                          error, ', '.join(obs for obs, _ in batch))
                super(StdWXCalculate, self).do_calculations(data_dict, {obs: directives[obs] for obs, _ in batch})

        def calculate(batch):
            with contextlib.ExitStack() as stack:
                for provider in sorted({p for _, p in batch}, key=id):
                    stack.enter_context(native.provider_lock(provider))
                specs = [(obs, native.weather_specification(obs, self.db_manager, provider, data_dict['usUnits']))
                         for obs, provider in batch]
                for obs, spec in specs:
                    if obs == 'rainRate':
                        spec['parameters']['event_time'] = data_dict['dateTime']
                # Only the inputs; other packet fields may hold values the
                # native request cannot carry.
                names = {'dateTime', 'usUnits', 'interval'}.union(*(spec['inputs'] for _, spec in specs))
                record = {name: data_dict[name] for name in names
                          if name in data_dict and (data_dict[name] is None or isinstance(data_dict[name], (int, float)))}
                results = self.db_manager.connection.native.packet_native(self.db_manager.table_name, record, specs)
                for (obs, provider), result in zip(batch, results):
                    if result['status'] == 'cannot_calculate':
                        data_dict[obs] = None
                    elif result['status'] == 'calculated':
                        data_dict[obs] = result['value']
                        if type(provider).__name__ != 'PressureCooker' or obs == 'pressure':
                            native.apply_state(provider, obs, result['state'], self.db_manager)

        for observation, directive in directives.items():
            obs = str(observation)
            if directive != 'software' and not (directive == 'prefer_hardware' and data_dict.get(obs) is None):
                continue
            provider = native.scalar_provider(obs)
            if provider is not None:
                try:
                    native.weather_specification(obs, self.db_manager, provider, data_dict['usUnits'])
                except Exception:
                    # For example maxSolarRad next to an almanac this
                    # extension does not know.
                    provider = None
            if provider is None:
                # Custom XTypes can depend on any fields calculated earlier.
                flush()
                super().do_calculations(data_dict, {obs: directive})
            else:
                pending.append((obs, provider))
        flush()


class NativeServices(weewx.engine.StdService):
    def __init__(self, engine, config_dict):
        super().__init__(engine, config_dict)
        from . import xtypes
        from . import almanac
        import weewx.xtypes
        import weewx.almanac
        previous = set(map(id, weewx.xtypes.xtypes + weewx.almanac.almanacs))
        xtypes.register()
        self._xtypes = [p for p in weewx.xtypes.xtypes if id(p) not in previous]
        self._almanacs = [p for p in weewx.almanac.almanacs if id(p) not in previous]

    def shutDown(self):
        import weewx.xtypes
        import weewx.almanac
        for registry, providers in [(weewx.xtypes.xtypes, self._xtypes), (weewx.almanac.almanacs, self._almanacs)]:
            registry[:] = [p for p in registry if p not in providers]
