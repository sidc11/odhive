# OdHive

> **The daily driver for Odoo developers. Your Odoo servers, one hive.**

[![CI](https://github.com/sidc11/odhive/actions/workflows/ci.yml/badge.svg)](https://github.com/sidc11/odhive/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

OdHive is a local web app, backed by a single Rust binary, for running and managing
Odoo development servers on your laptop: projects, multiple instances, `odoo.conf`
builder, Smart Reload, dependency auto-fix, database manager and a live log viewer.

**Status:** early development, working towards v0.1.0. Not ready for use yet.

## Scope (v0.1)

- Odoo **19** and **20**
- **Linux** and **macOS**
- Binds to `127.0.0.1` only; UI on port `9001` by default

## Building from source

Requirements: Rust (stable), Node.js 20+ and pnpm.

```sh
pnpm --dir web install
pnpm --dir web build
cargo run -p odhive
```

Then open <http://localhost:9001>.

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). All commits must be signed off (DCO).

## Security

Please report vulnerabilities privately — see [SECURITY.md](SECURITY.md).

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

## Author

Developed by **Siddharth Chaudhari** ([@sidc11](https://github.com/sidc11)).
