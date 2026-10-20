"""Native interval layout against weeutil.weeutil.intervalgen()."""

import os
import time

import pytest

weewxdb = pytest.importorskip('weewxdb')
import weeutil.weeutil
from weewxdb import native


@pytest.fixture
def los_angeles():
    previous = os.environ.get('TZ')
    os.environ['TZ'] = 'America/Los_Angeles'
    time.tzset()
    yield
    if previous is None:
        os.environ.pop('TZ', None)
    else:
        os.environ['TZ'] = previous
    time.tzset()


@pytest.mark.parametrize('start, stop, interval', [
    # Both DST changes of 2009, on and off the hour, from intervalgen()'s docstring.
    (1236477600, 1236538800, 10800), (1257051600, 1257080400, 3600),
    (1236477600 + 1800, 1236538800, 3600), (1257051600 - 2700, 1257080400 + 900, 1800),
    (1236000000, 1237000000, 86400), (1257000000, 1258000000, 3 * 86400),
])
def test_intervals_match_intervalgen(los_angeles, start, stop, interval):
    expected = [(int(span.start), int(span.stop))
                for span in weeutil.weeutil.intervalgen(start, stop, interval)]
    assert native.intervals(start, stop, interval) == expected
