# Security Policy

## Supported versions

Security fixes are provided for the latest published release. Older releases
may be used to reproduce a report, but users should update to the newest
version once a fix is available.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| Older releases | No |

## Reporting a vulnerability

Please do not disclose a suspected vulnerability in a public issue. Use
[GitHub's private vulnerability reporting form](https://github.com/ynjmxn/gengchou/security/advisories/new)
instead. If the form is unavailable, open a public issue containing no
security details and ask the maintainer for a private contact channel.

Include, when possible:

- the affected Gengchou version and Windows version;
- a concise description of the impact and the conditions required to trigger it;
- reproducible steps or a minimal proof of concept;
- whether credential handling, the self-updater, startup persistence, or file
  permissions are involved; and
- any suggested mitigation.

Do not include access tokens, cookies, provider credentials, private usage
data, or unredacted diagnostic logs. Reports are handled on a best-effort
basis; the maintainer will coordinate disclosure after the issue and a safe
upgrade path have been assessed.

Provider outages, account access, quota calculations, billing, and upstream
API behavior are normally outside this project's security scope unless
Gengchou introduces an additional vulnerability.
