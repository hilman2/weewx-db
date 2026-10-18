"""Load an extension archive and exercise its database and CLI without WeeWX."""

import argparse
import hashlib
import importlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile


def check(archive: Path) -> None:
    checksum = archive.with_suffix('.zip.sha256').read_text().split()[0]
    if hashlib.sha256(archive.read_bytes()).hexdigest() != checksum:
        raise ValueError('Archive checksum mismatch')
    with tempfile.TemporaryDirectory(prefix='weewxdb-smoke-') as directory:
        root = Path(directory)
        with zipfile.ZipFile(archive) as bundle:
            bundle.extractall(root)
        package_root = next(root.iterdir())
        sys.path.insert(0, str(package_root / 'bin/user'))
        try:
            weewxdb = importlib.import_module('weewxdb')
            native = weewxdb._weewxdb
            database = root / 'smoke.wwdb'
            native.create(str(database))
            with weewxdb.connect(database) as connection:
                connection.execute('CREATE TABLE archive (dateTime INTEGER PRIMARY KEY, usUnits INTEGER, interval INTEGER, outTemp REAL)').close()
                connection.execute('INSERT INTO archive VALUES (1704067200,17,5,12.3)').close()
                assert connection.execute('SELECT outTemp FROM archive').fetchone() == (12.3,)
            assert weewxdb.astronomy(1704067200, 52, 13)['distance_au'] > 0
            executable = package_root / 'bin/user/weewxdb/weewx-db'
            os.chmod(executable, 0o755)
            subprocess.run([str(executable), 'info', str(database)], check=True, stdout=subprocess.DEVNULL)
        finally:
            sys.path.pop(0)
    print('Verified %s on %s' % (archive.name, sys.version.split()[0]))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    args = parser.parse_args()
    check(args.archive.resolve())


if __name__ == '__main__':
    main()
