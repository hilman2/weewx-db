"""Compare the output directories of two report runs, such as SQLite and the Rust driver.

JSON documents must match after parsing, apart from the index's wall-clock
'rebuilt' time. HTML, XML and text files must match apart from decimal numbers,
which are compared with a relative tolerance: summing in another order changes
the last digits of a total.
"""

import argparse
import json
import math
import re
import sys
from pathlib import Path

NUMBER = re.compile(r'-?\d+\.\d+(?:e-?\d+)?')


def json_documents(root):
    result = {}
    for path in sorted(root.rglob('*.json')):
        value = json.loads(path.read_text())
        if path.name == 'index.json' and isinstance(value, dict):
            value.pop('rebuilt', None)
        result[str(path.relative_to(root))] = value
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('expected', type=Path)
    parser.add_argument('actual', type=Path)
    parser.add_argument('--ignore', action='append', default=[],
                        help='skip files whose relative path contains this text')
    parser.add_argument('--tolerance', type=float, default=1e-12)
    options = parser.parse_args()
    differing = []
    expected, actual = json_documents(options.expected), json_documents(options.actual)
    for name in sorted(set(expected) | set(actual)):
        if not any(text in name for text in options.ignore) and expected.get(name) != actual.get(name):
            differing.append(name)
    for path in sorted(options.expected.rglob('*')):
        name = str(path.relative_to(options.expected))
        if path.suffix not in ('.html', '.xml', '.txt') or any(text in name for text in options.ignore):
            continue
        other = options.actual / name
        first, second = path.read_text(), other.read_text() if other.exists() else ''
        numbers = list(zip(NUMBER.findall(first), NUMBER.findall(second)))
        if NUMBER.sub('#', first) != NUMBER.sub('#', second) or not all(
                math.isclose(float(a), float(b), rel_tol=options.tolerance) for a, b in numbers):
            differing.append(name)
    print('%d JSON documents, %d differing files' % (len(expected), len(differing)))
    for name in differing[:20]:
        print('  ' + name)
    return 1 if differing else 0


if __name__ == '__main__':
    sys.exit(main())
