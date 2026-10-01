# Security policy

## Supported versions

Only the latest release of Jellybeam is supported. Please update before
reporting an issue that might already be fixed.

## What's at stake

Jellybeam stores Jellyfin (and, if configured, Jellyseerr/Overseerr) access
tokens on the Mac, in a file under Application Support with owner-only
permissions or in the Keychain. It also treats the media server as untrusted
input: a malicious or compromised server should not be able to crash the
client, read files it should not, or execute code. A vulnerability in either
class is a security report.

## Reporting a vulnerability

Please report privately rather than opening a public issue: on this
repository's GitHub page, go to the **Security** tab and choose **Report a
vulnerability**.

Include what you found, how to reproduce it, and its impact if you can.
Please don't include real server addresses, tokens, or other identifying
details in the report; describe them generically or redact them.

We aim to respond within 14 days.
