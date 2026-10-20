# Install the WeeWX database extension

The extension installs the Rust database, SQL CLI, weedb driver, XTypes and
solar/lunar almanac together on WeeWX 5. It does not include a JSON generator.
JSON reports use the generator that ships with WeeWX 5.6. The bundled
subclass of it is optional and cannot be enabled on earlier releases.

## Choose a release package

Download the ZIP matching your machine from the repository's
[releases](https://github.com/hilman2/weewx-db/releases). Each ZIP contains the
Rust module, SQL CLI, Python adapters and matching sources.

| Target in the archive name | System |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Linux on Intel or AMD, including NUCs |
| `aarch64-unknown-linux-gnu` | Linux on ARM64, including 64-bit Raspberry Pi OS |
| `armv7-unknown-linux-gnueabihf` | Linux on ARMv7, including 32-bit Raspberry Pi OS on Pi 2 or newer |
| `x86_64-apple-darwin` | macOS on Intel |
| `aarch64-apple-darwin` | macOS on Apple Silicon |

Linux release packages require glibc 2.17 or later. The Rust module uses
CPython's stable ABI for Python 3.9 and later. Choose the architecture of the
Python interpreter that runs WeeWX. A Pi running a 32-bit userspace needs ARMv7
even when its kernel is 64-bit. ARMv6 machines such as the original Pi and Pi
Zero are outside this release matrix.

## Verify and install

Verify the downloaded archive against the release's `SHA256SUMS`, then install
it with `weectl`. For example:

```sh
sha256sum --ignore-missing --check SHA256SUMS
weectl extension install weewx-db-0.2.0-aarch64-unknown-linux-gnu.zip
```

On macOS, run `shasum -a 256 ARCHIVE.zip` and compare the printed hash with
`SHA256SUMS`.

Installation needs neither a Rust compiler nor pip. The installer refuses an
archive built for another system or architecture, so a mismatched download
cannot break the installation.

The installer places the extension under `bin/user/weewxdb` in your WeeWX root
and adds a `weewx_rust` entry to `[Databases]` in `weewx.conf`. WeeWX keeps
running on SQLite until you change a binding.

## Import and select your database

The [project README](https://github.com/hilman2/weewx-db#put-it-into-operation)
walks through stopping WeeWX, importing the SQLite archive, pointing
`wx_binding` at `weewx_rust`, checking the first records and switching back.

## Choose stored or calculated values in JSON reports

JSON reports need no change to read from the Rust database. WeeWX 5.6's
`weewx.jsongenerator.JSONGenerator` reaches it through the driver.

The extension's subclass of that generator adds a per-line `source` policy. It
also calculates the day/night files from the Rust solar ephemeris. To use it,
replace `weewx.jsongenerator.JSONGenerator` in `[Generators] generator_list` of
the report's `skin.conf` with `user.weewxdb.json.JSONGenerator`. Keep the other
generators in the list. Configure the report through WeeWX's existing
`[JSONGenerator]` options.

A line can select `source = derived` to calculate values even when the schema
contains the observation, or `source = fill_missing` to keep stored non-NULL
values and calculate the gaps. The default `auto` follows ordinary WeeWX
selection. Explicit source policies use archive records.
See the [batch API reference](batch.md#weewx-56-json-integration) for the
supported aggregates and the compatibility check.

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

## Build from source

Release packages cover the targets in the table above. Build your own archive
when you need another platform or a local change. Build locally with Rust and
Python 3.9 or later:

```sh
python3 tools/build_extension.py
```

Use the archive name printed by the command. Local native builds use the host's
system libraries, so their glibc requirement can be newer than the CI packages.
For a portable Linux build, install cargo-zigbuild and the Rust target:

```sh
python3 -m pip install cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu
python3 tools/build_extension.py --target aarch64-unknown-linux-gnu --glibc 2.17
```

The packager checks the architecture and glibc symbols in both native binaries
before labeling the archive. Each archive includes its sources under `source/`
and target information in `bin/user/weewxdb/build.json`.
`python3 tools/check_extension.py ARCHIVE.zip` loads a built archive in a fresh
interpreter.

## Build with GitHub Actions

[Build release packages](https://github.com/hilman2/weewx-db/actions/workflows/release.yml)
builds all five targets. It can be started manually from Actions. Manual runs
retain ZIPs and their SHA-256 checksums as workflow artifacts.

Pushing a `v*` tag runs the same builds and attaches the packages to a draft
release after all targets pass. The tag must equal `v` followed by the version
in both `Cargo.toml` and `pyproject.toml`. Publication of the draft is a separate
step. Ordinary branch pushes and pull requests do not start release builds.

Each target loads its native module on Python 3.9 and 3.13, writes and reads a
database, calculates a solar position, and runs the bundled CLI. ARMv7 runtime
checks use QEMU; the other targets use matching runner hardware. These checks
verify the built artifacts. The full differential test suite remains local.
