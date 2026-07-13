# Security policy

## Supported version

Security fixes currently target TurboMerger 7.6.x.

## Reporting a vulnerability

Use the repository's private vulnerability-reporting feature when available:

<https://github.com/cryptofan500/TurboMerger/security/advisories/new>

Do not place credentials, private source code, exploit payloads containing real
secrets, or personal data in a public issue. If private reporting is unavailable,
open a minimal public issue asking the maintainer for a private contact channel.

## Safe use

- Keep redaction and gitignore handling enabled unless you understand the risk.
- Review every generated snapshot before sending it to a third-party model.
- Test apply-back on a disposable copy and review every proposed file.
- Verify release checksums. macOS friend builds are ad-hoc signed, not notarized.
- Never treat pattern-based redaction as a substitute for credential rotation or a
  dedicated secret scanner.
