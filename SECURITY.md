# Security Policy

## Supported versions

kmsrdp is currently experimental. Security fixes are provided for the
latest release and the current `main` branch only.

## Reporting a vulnerability

Please do not disclose suspected vulnerabilities in a public issue,
discussion, or pull request.

Use GitHub's **Report a vulnerability** button on this repository's
Security tab to submit a private security advisory. Include:

- the affected version or commit;
- the deployment environment and client used;
- reproduction steps or a proof of concept;
- the expected impact; and
- any suggested mitigation, if known.

You should receive an acknowledgement within seven days. Please allow time
for a fix and coordinated release before publishing details.

## Deployment warning

kmsrdp provides authenticated clients with complete screen visibility,
keyboard and mouse control, and optional clipboard, audio, and redirected
drive access (client drives appear under the session user's
`$XDG_RUNTIME_DIR/kmsrdp/drives/`). It supports optional NLA
(CredSSP/NTLMv2; no Kerberos) and uses a persisted self-signed TLS
certificate by default (under systemd `StateDirectory=kmsrdp`, or paths
from `KMSRDP_TLS_*`). Set `KMSRDP_TLS_EPHEMERAL=1` to regenerate every
start instead.

Do not expose it directly to the public Internet. Restrict the RDP listen
address (defaults `0.0.0.0:3389`; `KMSRDP_BIND` / `KMSRDP_PORT`) to trusted
clients and use a trusted LAN, VPN, or SSH tunnel.
