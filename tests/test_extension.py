"""Install and uninstall the complete archive in a temporary WeeWX station."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import zipfile

import configobj
import pytest
import weecfg.extension
import weewx
import weewx.manager

ROOT = Path(__file__).resolve().parents[1]


def build_archive(output):
    specification = importlib.util.spec_from_file_location('build_extension', ROOT / 'tools/build_extension.py')
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    return module.build(output, compile_rust=False)


def test_real_extension_install_offline_import_cli_and_uninstall(tmp_path):
    archive = build_archive(tmp_path / 'dist')
    with zipfile.ZipFile(archive) as bundle:
        assert any(name.endswith('/source/build.rs') for name in bundle.namelist())
        assert any(name.endswith('/source/src/analytics.rs') for name in bundle.namelist())
        assert any(name.endswith('/source/tools/build_extension.py') for name in bundle.namelist())
        assert not any(name.endswith('/weewx/jsongenerator.py') for name in bundle.namelist())
    station = tmp_path / 'station'
    (station / 'bin/user').mkdir(parents=True)
    (station / 'bin/user/__init__.py').write_text('')
    configuration = configobj.ConfigObj({
        'WEEWX_ROOT': str(station), 'StdReport': {'HTML_ROOT': 'public_html', 'SKIN_ROOT': 'skins'},
        'Engine': {'Services': {name: [] for name in weewx.all_service_groups}},
        'DataBindings': {'wx_binding': {'database': 'archive_sqlite'}},
        # Package installations keep SQLite databases outside WEEWX_ROOT.
        'DatabaseTypes': {'SQLite': {'driver': 'weedb.sqlite', 'SQLITE_ROOT': str(tmp_path / 'var-lib-weewx')}},
        'Databases': {'archive_sqlite': {'driver': 'weedb.sqlite', 'database_name': 'original.sdb'}}}, interpolation=False)
    configuration.filename = str(station / 'weewx.conf')
    configuration.write()
    engine = weecfg.extension.ExtensionEngine(configuration.filename, configuration)
    engine.install_extension(str(archive), no_confirm=True)
    assert configuration['DataBindings']['wx_binding']['database'] == 'archive_sqlite'
    assert configuration['Databases']['weewx_rust']['driver'] == 'user.weewxdb.driver'
    # The database opens with its own driver and the station's SQLite directory.
    database = weewx.manager.get_database_dict_from_config(configuration, 'weewx_rust')
    assert database['driver'] == 'user.weewxdb.driver'
    assert database['SQLITE_ROOT'] == str(tmp_path / 'var-lib-weewx')
    # Belchertown chooses its SQL dialect from the driver of database_type.
    database_type = configuration['Databases']['weewx_rust']['database_type']
    assert configuration['DatabaseTypes'][database_type]['driver'] == 'weedb.sqlite'
    assert configuration['Engine']['Services']['xtype_services'] == ['user.weewxdb.services.NativeServices']
    runtime = station / 'bin/user/weewxdb'
    assert not (runtime / 'jsongenerator.py').exists()
    code = '''
import sys
sys.path.insert(0, sys.argv[1])
sys.path.insert(0, sys.argv[2])
from user.weewxdb import driver, astronomy
from user.weewxdb.services import NativeServices
import weewx.manager
manager = weewx.manager.Manager.open_with_create({'driver':'user.weewxdb.driver', 'database_name':sys.argv[3]}, schema=[('dateTime','INTEGER PRIMARY KEY'),('usUnits','INTEGER'),('interval','INTEGER'),('outTemp','REAL')])
manager.addRecord({'dateTime':1704067200,'usUnits':17,'interval':5,'outTemp':12.3})
assert manager.getRecord(1704067200)['outTemp'] == 12.3
assert astronomy(1704067200,52,13)['distance_au'] > 0
manager.close()
'''
    source = str(Path(weewx.__file__).resolve().parents[1])
    result = subprocess.run([sys.executable, '-I', '-c', code, source, str(station / 'bin'), str(station / 'data.wwdb')],
                            capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    result = subprocess.run([str(runtime / 'weewx-db'), 'info', str(station / 'data.wwdb')], capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    engine.uninstall_extension('weewx-db', no_confirm=True)
    assert not (runtime / '_weewxdb.abi3.so').exists()
    assert not (runtime / 'weewx-db').exists()
    assert (station / 'data.wwdb').exists()
    assert configuration['Databases']['archive_sqlite']['database_name'] == 'original.sdb'
    assert not configuration['Engine']['Services']['xtype_services']


def test_wrong_platform_fails_before_installing_files(tmp_path):
    archive = build_archive(tmp_path / 'dist')
    with zipfile.ZipFile(archive) as bundle:
        bundle.extractall(tmp_path / 'extracted')
    directory = next((tmp_path / 'extracted').iterdir())
    installer = directory / 'install.py'
    source = installer.read_text()
    line = next(line for line in source.splitlines() if line.startswith('PLATFORM = '))
    installer.write_text(source.replace(line, "PLATFORM = ('OtherOS', 'other-machine')"))
    # WeeWX imports installers as a module named install.
    specification = importlib.util.spec_from_file_location('wrong_platform', installer)
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    with pytest.raises(weecfg.extension.InstallError, match='build an archive on the target system'):
        module.loader()
