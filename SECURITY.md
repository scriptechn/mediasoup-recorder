# Security

Please report a vulnerability privately, not in a public issue: use **Report a vulnerability** under the repository's
Security tab (GitHub private vulnerability reporting). You will get an answer there.

## What to know when deploying

- The control socket is protected by one shared secret (`RECORDER_SECRET`) sent in the handshake. Keep the control port
  on a private network or behind TLS; anyone who can connect with the secret can start captures and fill the spool.
- RTP between the SFU and the recorder is not encrypted. Run them on the same host or a private network.
- The recorder writes whatever the controller tells it to record. Consent, authorisation and access to the finished
  files are the application's responsibility.
