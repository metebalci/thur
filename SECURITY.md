# Security Policy

Thur VTL and Thur VSA handle stored data, encryption keys, and
storage-network credentials. Security reports are taken seriously —
this document explains how to report a vulnerability and what to
expect.

## Supported versions

Only the `main` branch is supported. Security fixes land on `main`;
they are not backported to tagged pre-1.0 (`alpha`) releases. If you
run a tagged release, update to current `main` (or the next release
cut from it) to receive a fix.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Report it privately through GitHub: on the repository's **Security**
tab, use **"Report a vulnerability"** to open a private advisory
([GitHub Private Vulnerability Reporting][pvr]). The report stays
private until a fix is published.

[pvr]: https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability

## What to include

A useful report has:

- **Product** — Thur VTL or Thur VSA (or shared code affecting both).
- **Version** — the `--version` string from the affected binary.
- **Deployment** — transport (iSCSI / NVMe-TCP), cloud backend in
  use, and host OS.
- **Reproduction** — minimal steps to reproduce, and what an attacker
  gains.

## Response expectations

This is a solo-maintained project. Under normal conditions a report
is acknowledged within about **5 business days**; outside normal
conditions, handling is best-effort. After triage, a fix timeline is
communicated in the private advisory thread.

## Coordinated disclosure

Please keep a reported issue private until a fix is published or 90
days have passed, whichever comes first. Reporters are credited in
the published advisory by default — say so in the report if you
prefer to remain anonymous.

## Scope

In scope:

- The `thurvtld` / `thurvsad` daemons and the
  `thurvtl` / `thurvsa` tools.
- The iSCSI and NVMe-TCP wire surface.
- Encryption and key handling — tape AME, per-volume DEKs, keystore
  backends.
- Audit-log chain integrity.
- Credential handling — CHAP / mutual-CHAP, NVMe-TCP TLS-PSK, cloud
  backend credentials, the `<product>.env` file.

Out of scope:

- Vulnerabilities in third-party dependencies — report those
  upstream. Dependency advisories are tracked here via Dependabot.
- Operator misconfiguration and missing OS-level hardening.
- Findings that require an already-compromised host or root access.
- Theoretical issues with no demonstrated practical impact.

## Current security posture

- This is **alpha software**. It has not had an independent security
  audit; one will be planned after the 1.0 GA release.
- The admin **HTTP endpoint** (`/metrics`, `/health`, default port
  9090) is **unauthenticated by design** and intended only for a
  local Prometheus scrape. Do not expose it to untrusted networks;
  authenticated / TLS-protected admin HTTP is not yet implemented.
  Mutating operations go through a separate Unix-socket admin channel
  that is peer-credential authenticated and local-only.
