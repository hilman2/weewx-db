# Sources and licenses

This project is licensed under GPL-3.0-only. The full license is in [LICENSE](LICENSE).

The initial snapshot was developed in the [hilman2/weewx](https://github.com/hilman2/weewx)
checkout at `def717274ef07da1910f9a0975f8d76ed287aa47`. WeeWX's calculation
and database semantics are the reference for the native adapters and tests.
The original experiment's file hashes are recorded in [SOURCE_SNAPSHOT.json](SOURCE_SNAPSHOT.json).

The solar/lunar numerical models derive from PyEphem/XEphem and S. L. Moshier's
work. Their MIT notice is retained in [vendor/LICENSE.ephem](vendor/LICENSE.ephem).
The [astronomy reference](astronomy.md) identifies the model and import procedure.

Historical and threshold aggregation semantics follow Tom Keffer's
[weewx-xaggs](https://github.com/tkeffer/weewx-xaggs), copyright 2023-2024.
Its LGPL terms are retained in [vendor/LICENSE.xaggs](vendor/LICENSE.xaggs).

Public reference repository URLs and exact revisions are in
[references.json](references.json). Downloaded reference checkouts and synthetic
database fixtures are development inputs and are excluded from this repository.
