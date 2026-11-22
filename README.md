<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/odhive-wordmark-dark.svg">
    <img src="assets/logo/odhive-wordmark.svg" alt="OdHive" height="72">
  </picture>
</h1>

> **The daily driver for Odoo developers. Your Odoo servers, one hive.**

[![CI](https://github.com/sidc11/odhive/actions/workflows/ci.yml/badge.svg)](https://github.com/sidc11/odhive/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

OdHive is a local web app, backed by a single Rust binary, for running and managing
Odoo development servers on your laptop: projects, multiple instances, `odoo.conf`
builder, Smart Reload, dependency auto-fix, database manager and a live log viewer.

**Status:** early development, working towards v0.1.0. Not ready for use yet.

## Scope (v0.1)

- Odoo **19** and **20**
- **Linux** — every distro family Odoo supports: Debian / Ubuntu (deb) and Fedora / RHEL / Rocky / Alma (rpm); x86_64 and arm64. Release binaries are static (musl), so they don't depend on the system glibc.
- **macOS**
- Binds to `127.0.0.1` only; UI on port `7069` by default

## Building from source

Requirements: Rust (stable), Node.js 20+ and pnpm.

```sh
pnpm --dir web install
pnpm --dir web build
cargo run -p odhive
```

`odhive` starts the OdHive service in the background (one per user) if it
isn't running and signs your browser in to <http://localhost:7069>. The
service keeps running when the browser is closed.

```sh
odhive                  # start the service if needed, open the browser
odhive --no-browser     # print a one-time sign-in link instead
odhive serve            # start the service only
odhive serve --foreground   # run it in this terminal until Ctrl+C
odhive quit             # stop the service and the instances it runs
odhive service install  # start it at login (systemd user unit / launchd agent)
odhive service uninstall
```

With `odhive service install`, systemd (Linux) or launchd (macOS) owns the
service: it starts at login, and `odhive` starts it through them. The
service gets the `PATH` of the shell you install from, so it finds the same
`uv`, `git` and Python. Its log is in `journalctl --user -u odhive` on Linux
and in `service.log` in the OdHive data folder on macOS.

## Command line (preview)

`odhive start` still runs instances in the foreground and stops them on
Ctrl+C (it moves onto the service soon). The other commands work from any
terminal.

```sh
odhive source add ~/src/odoo                 # or: odhive source clone 19.0
odhive project add shop --source odoo-19.0   # folders, venv, requirements
odhive start shop                            # every instance; shop#test for one
odhive status
odhive logs shop -f
odhive stop shop
```

Instances listen on `127.0.0.1` unless the project sets `http_interface`.
Output is also written to `projects/<name>/logs/<instance>.log`.

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). All commits must be signed off (DCO).

## Security

Please report vulnerabilities privately — see [SECURITY.md](SECURITY.md).

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

## Author

Developed by **Siddharth Chaudhari** ([@sidc11](https://github.com/sidc11)).
