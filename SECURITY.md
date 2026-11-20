# Security

Please report vulnerabilities **privately** through
[GitHub's security advisories](https://github.com/Layfate/Gitust/security/advisories/new),
not as a public issue. Include what an attacker controls (for example a
repository someone is tricked into opening) and what they can achieve.

Areas that matter most:

- anything that makes Gitust run a program, or overwrite or delete files, from a
  repository the user only opened or browsed;
- staging, discarding or committing different content than the user selected;
- leaking the GitHub token (it should only ever live in the OS credential vault).

How Gitust defends against these is described under "Security notes" in the README.
Only the latest release receives fixes.
