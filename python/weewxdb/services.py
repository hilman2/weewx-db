"""Optional service replacement that calculates ordered packet fields in Rust."""

import contextlib

import weewx.engine
import weewx.wxservices

from . import native


class StdWXCalculate(weewx.wxservices.StdWXCalculate):
    def do_calculations(self, data_dict, calc_dict=None):
        directives = self.archive_calc_dict if calc_dict is None else calc_dict
        if getattr(self.db_manager.connection, 'native', None) is None:
            return super().do_calculations(data_dict, directives)
        pending = []

        def flush():
            if not pending:
                return
            with contextlib.ExitStack() as stack:
                for provider in sorted({p for _, p in pending}, key=id):
                    stack.enter_context(native.provider_lock(provider))
                specs = [(obs, native.weather_specification(obs, self.db_manager, provider, data_dict['usUnits']))
                         for obs, provider in pending]
                for obs, spec in specs:
                    if obs == 'rainRate':
                        spec['parameters']['event_time'] = data_dict['dateTime']
                record = {name: value for name, value in data_dict.items()
                          if value is None or isinstance(value, (int, float))}
                results = self.db_manager.connection.native.packet_native(self.db_manager.table_name, record, specs)
                for (obs, provider), result in zip(pending, results):
                    if result['status'] == 'cannot_calculate':
                        data_dict[obs] = None
                    elif result['status'] == 'calculated':
                        data_dict[obs] = result['value']
                        if type(provider).__name__ != 'PressureCooker' or obs == 'pressure':
                            native.apply_state(provider, obs, result['state'], self.db_manager)
            pending.clear()

        for observation, directive in directives.items():
            obs = str(observation)
            if directive != 'software' and not (directive == 'prefer_hardware' and data_dict.get(obs) is None):
                continue
            provider = native.scalar_provider(obs)
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
