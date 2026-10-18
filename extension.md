# Install the WeeWX database extension

The extension installs the Rust database, SQL CLI, weedb driver, XTypes and
solar/lunar almanac together. The JSON adapter uses the generator supplied by
WeeWX 5.6. The database and XTypes also work without that generator.

## Build and install

Build on the machine where you will run WeeWX, using Rust and Python 3.9 or later:

```sh
python3 tools/build_extension.py
weectl extension install dist/weewx-db-0.2.0-linux-x86_64.zip
```

Use the archive name printed by the build command. A Raspberry Pi needs an
archive built for its own OS and architecture. Installation from that archive
needs neither a Rust compiler nor pip. Each archive also includes matching
sources under `source/`, which can be rebuilt on another architecture.
The current Linux archive uses the host's
system libraries, so an archive built on a newer distribution may need rebuilding
on an older target. See the included `build.json` for its build platform.

## Import and select your database

The installer adds `Databases.weewx_rust`. Stop WeeWX, then import the SQLite
archive into a new file. Replace the example paths with your station paths:

```sh
/home/weewx/bin/user/weewxdb/weewx-db import /home/weewx/archive/weewx.sdb /home/weewx/archive/weewx.wwdb
```

Set the intended binding in `weewx.conf`:

```ini
[DataBindings]
    [[wx_binding]]
        database = weewx_rust
```

Start WeeWX. Other databases and bindings can continue to use SQLite. SQL
queries use the bundled CLI, for example:

```sh
/home/weewx/bin/user/weewxdb/weewx-db sql /home/weewx/archive/weewx.wwdb 'SELECT max(outTemp) FROM archive'
```

## Enable the JSON adapter

In the report's `skin.conf`, replace `weewx.jsongenerator.JSONGenerator` in
`[Generators] generator_list` with `user.weewxdb.json.JSONGenerator`. Keep the
other generators in the list. Configure the report through WeeWX's existing
`[JSONGenerator]` options.

A line can select `source = derived` to calculate values even when the schema
contains the observation, or `source = fill_missing` to keep stored non-NULL
values and calculate the gaps. The default `auto` follows ordinary WeeWX
selection. Explicit source policies use archive records.
See [batch API reference](batch.md) for supported aggregates.

## Enable packet calculations

In `weewx.conf`, replace `weewx.wxservices.StdWXCalculate` in
`[Engine][[Services]] process_services` with
`user.weewxdb.services.StdWXCalculate`, keeping its position. The existing
`[StdWXCalculate]` directives still control software and hardware calculations.

## Remove the extension

Switch bindings back to an available SQLite archive. Restore the two generator
and packet-service class names above if you changed them, then run:

```sh
weectl extension uninstall weewx-db
```

Database files remain on disk. To transfer new observations back to SQLite,
use the bundled CLI's `export` command before uninstalling.
