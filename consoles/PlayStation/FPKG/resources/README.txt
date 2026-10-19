Sony PS4 update-feed TLS trust anchor

sony-dnas-root-05.pem is the public SCEI DNAS Root 05 CA certificate used by
Sony's PS4 update-feed hostname. Digital Forge trusts it only in its PS4
update clients, in addition to normal certificate and hostname validation.
The application checks the DER SHA-256 fingerprint before adding this root:

  51d5a7833f67f1d1c6212f75997e4b83dada0f7910ba4b5a3fa4e9b7b9bc827c

The fingerprint is independently published by the PS3 Tools project's
PS4/Sony update client, which also verifies this root before making an HTTPS
request:

  https://github.com/setsid/ps3-tools/blob/main/ps3tools/updates.py

This root is not installed into Windows or used by other Digital Forge HTTP
clients. Do not replace the pinned fingerprint or disable TLS validation to
work around a mismatch.
