# Security Policy

## Supported versions

OdHive is pre-1.0. Only the latest release receives security fixes.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report vulnerabilities privately through GitHub:
**Security → Report a vulnerability** on <https://github.com/sidc11/odhive/security/advisories/new>.

Please include:

- affected version or commit
- steps to reproduce or a proof of concept
- the impact you expect

You should receive an acknowledgement within 7 days. Once a fix is available, a
GitHub Security Advisory will be published and you will be credited unless you
prefer otherwise.

## Scope

OdHive is a local developer tool bound to `127.0.0.1`. Issues of particular interest:

- DNS rebinding, CSRF or cross-origin access to the local API / WebSocket
- authentication token leakage or bypass
- command or argument injection when spawning processes
- path traversal, zip-slip or zip bombs during restore
- SQL injection via database names
- secrets exposed in the UI, API, logs or debug bundles
