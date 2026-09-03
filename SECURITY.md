# Security policy

## Reporting a vulnerability

Report privately through GitHub's **Private Vulnerability Reporting**, not in a
public issue: open
<https://github.com/metaneutrons/devserial/security/advisories/new>. That keeps
the report visible only to you and the maintainer until a fix exists, and it
gives us a place to coordinate a CVE if one is warranted.

Do not open a public issue, a pull request or a discussion for a security
problem, and do not post a proof of concept publicly before a fix is released.

If Private Vulnerability Reporting is unavailable to you, say so in a public
issue **without any detail about the problem** and a private channel will be
arranged.

## What to include

- The version, from `devserial --version`, or the commit if you built from source
- The platform and how devserial was installed
- What an attacker can do, and what they need to be able to do it
- Reproduction steps, ideally the smallest case that still shows the problem

## Response

- Acknowledgement within 5 working days
- An assessment, with a severity and a plan, within 10 working days
- A fix released as a patch version, with the advisory published at the same time

These are targets for a project maintained by one person, not a contractual
commitment. If a report goes unacknowledged past those windows, a public issue
saying only that a security report is awaiting a reply is appropriate.

## Supported versions

Only the latest release receives fixes. There are no maintenance branches for
older versions; the fix for any reported problem is the next patch release.

## Scope

In scope is anything that lets someone who should not have access read or write
a serial port, the SQLite capture, the daemon socket or the named pipe, plus
anything that turns device output into code execution.

Out of scope is what devserial deliberately does: whoever may talk to the daemon
may drive every port the daemon holds. On Unix the socket is created with mode
0600 and the peer's uid is checked against the socket owner; on Windows the pipe
rejects remote clients. A user who is already able to open the port directly is
not a privilege boundary devserial claims to defend.
