---
title: Reporting security issues
description: Report a vulnerability in firebrick privately, by email.
---

Firebrick keeps coding agents inside a sandbox, so a flaw that lets code in a
sandbox reach the host, leak a secret or get past the network policy is the most
important kind of bug it can have. Please report it privately, so it can be
fixed before anyone can use it.

## How to report

**Don't report security issues in public.** Don't open a GitHub issue, pull
request or discussion, and don't post details anywhere else before a fix is
released.

Email your report to
[willem.meints@gmail.com](mailto:willem.meints@gmail.com?subject=%5Bfirebrick%20security%5D)
with `[firebrick security]` in the subject. Include as much of this as you can:

- The firebrick version (`fbk --version`) and your host operating system.
- What an attacker can do, and from where: code in the sandbox, a malicious
  repository, another local user or a network attacker.
- Steps to reproduce, such as a `.firebrick.yml`, the commands you ran and the
  output you saw.
- A proof of concept, if you have one.
- Whether, and how, you want to be credited.

## What happens next

Firebrick is maintained by one person in their spare time, so these are goals,
not guarantees:

- You get an acknowledgement within 5 working days.
- Within 14 days you hear whether it's a vulnerability and how it will be fixed.
- Once a fix is released, a GitHub security advisory credits you, unless you
  prefer to stay anonymous.

Please wait 90 days, or until a fix is released if that's sooner, before you
disclose the issue publicly.

Firebrick has no bug bounty program and can't pay for reports. Security fixes go
into the next release only, so upgrade to the latest release to get them.

## What's in scope

In short: anything that breaks the promises of the sandbox. That includes code
in a sandbox that reaches host files outside the workspace or runs code on the
host, secrets that reach a sandbox, traffic that gets past an enforced network
policy, and a `.firebrick.yml` that widens its own access without your approval.

Escapes from microsandbox itself, malware already running as your user, and
misuse of access you granted, such as an allowed host or a mounted directory,
are out of scope. The
[security policy](https://github.com/wmeints/firebrick/blob/main/SECURITY.md) in
the repository has the full list. When in doubt, report it anyway.
