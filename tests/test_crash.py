# Copyright (c) 2026 Manuel Hilgert <hilman2@gmail.com>
"""Killed writers lose nothing they acknowledged, and the file stays valid.

`just full` sets WEEWXDB_LONG_TESTS=1 for more rounds.
"""

import os
import pathlib
import random
import signal
import subprocess
import sys
import time

import weewxdb

ROOT = pathlib.Path(__file__).resolve().parents[1]
ROUNDS = 60 if os.environ.get('WEEWXDB_LONG_TESTS') == '1' else 8

# Inserts rows from argv[2] on, one per transaction, and prints each
# dateTime after its commit returned. Every 40th row folds the changes into
# blocks, so kills also land inside compactions.
WRITER = '''
import sys
import weewxdb
connection = weewxdb.connect(sys.argv[1], timeout=30, sync_mode='always')
time = int(sys.argv[2])
while True:
    time += 300
    connection.execute('INSERT INTO archive VALUES (?, ?)', (time, time / 1000.0)).close()
    print(time, flush=True)
    if time // 300 % 40 == 0:
        connection.native.checkpoint(False)
'''


def create(path):
    weewxdb._weewxdb.create(str(path))
    connection = weewxdb.connect(path)
    connection.execute('CREATE TABLE archive(dateTime INTEGER PRIMARY KEY, value REAL)').close()
    connection.close()


def check(path):
    result = subprocess.run([str(ROOT / 'target/release/weewx-db'), 'check', str(path)], capture_output=True, text=True)
    assert result.stdout.strip() == 'ok', result.stderr


def stored(path):
    # A new process reads the file as weewxd would after a restart.
    code = 'import sys, weewxdb; c = weewxdb.connect(sys.argv[1]); ' \
           'print(*[r[0] for r in c.execute("SELECT dateTime FROM archive ORDER BY dateTime")])'
    result = subprocess.run([sys.executable, '-c', code, str(path)], capture_output=True, text=True, check=True,
                            env=dict(os.environ, PYTHONPATH=os.pathsep.join(sys.path)))
    return set(map(int, result.stdout.split()))


def run_writer(path, start):
    return subprocess.Popen([sys.executable, '-c', WRITER, str(path), str(start)], stdout=subprocess.PIPE, text=True,
                            env=dict(os.environ, PYTHONPATH=os.pathsep.join(sys.path)), start_new_session=True)


def test_killed_writers_keep_acknowledged_rows(tmp_path):
    path = tmp_path / 'crash.wwdb'
    create(path)
    acknowledged = set()
    random.seed(7)
    for round_ in range(ROUNDS):
        # Two writers in their own processes, on separate time ranges.
        writers = [run_writer(path, 10 ** 9 * (k + 1) + 10 ** 6 * round_) for k in range(2)]
        time.sleep(random.uniform(0.05, 0.4))
        for writer in writers:
            os.killpg(writer.pid, signal.SIGKILL)
        for writer in writers:
            output, _ = writer.communicate()
            # The last line may be cut off by the kill.
            lines = output.splitlines()
            acknowledged.update(int(line) for line in lines[:-1])
        missing = acknowledged - stored(path)
        assert not missing, sorted(missing)[:5]
        check(path)
    assert len(acknowledged) > ROUNDS
