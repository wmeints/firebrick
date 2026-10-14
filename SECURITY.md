# Security policy

Firebrick exists to keep coding agents inside a sandbox, so a flaw that lets
code in a sandbox reach the host, leak a secret or bypass the network policy is
the most important kind of bug this project can have. Thank you for taking the
time to report one responsibly.

## Reporting a vulnerability

**Don't report security issues in public.** Don't open a GitHub issue, pull
request or discussion, and don't post details in comments, chat or social media
before a fix is released.

Email your report to **willem.meints@gmail.com** with `[firebrick security]` in
the subject. Include as much of the following as you can:

- The firebrick version (`fbk --version`) and your host operating system.
- What an attacker can do, and from where: code in the sandbox, a malicious
  repository, another local user or a network attacker.
- Steps to reproduce, such as a `.firebrick.yml`, the commands you ran and the
  output you saw.
- A proof of concept, if you have one.
- Whether, and how, you want to be credited.

If you believe the issue is being exploited already, say so in the subject.

## What happens next

Firebrick is maintained by one person in their spare time, so these are goals,
not guarantees:

- I acknowledge your report within 5 working days.
- I confirm whether it's a vulnerability, and tell you how I plan to fix it,
  within 14 days.
- I keep you informed of the progress and let you review the fix if you like.
- Once a fix is released, I publish a GitHub security advisory that credits you,
  unless you prefer to stay anonymous.

Please give me 90 days, or until a fix is released if that's sooner, before you
disclose the issue publicly. If a fix takes longer, we'll agree on a date
together.

## Bug bounty

Firebrick has no bug bounty program and can't pay for reports. I'm grateful for
every report and credit reporters in the advisory and the release notes.

## Supported versions

Firebrick is in early development. Security fixes go into the next release only;
older releases don't get patches. Upgrade to the latest release to get them.

| Version        | Supported |
| -------------- | --------- |
| Latest release | Yes       |
| Older releases | No        |

## Scope

The [threat model](docs/architecture/concepts/threat-model.md) describes what
firebrick protects, from whom, and the trust boundaries. In short, these are in
scope:

- Code in a sandbox that reads or changes host files outside the workspace and
  the approved mounts, or runs code on the host.
- Secret values or private keys that reach a sandbox.
- Traffic that gets past the network policy with `network.enforce: true`.
- A sandbox, or a repository's `.firebrick.yml`, that widens its own access
  without the user's approval.
- Another local user who can use your daemon, sandboxes, secrets or keys.
- Flaws in the release artifacts: the binaries, the install scripts and the
  `firebrick-base` image.

These are out of scope:

- Malware that runs as the host user, or a compromised host.
- Escapes from libkrun, the guest kernel or microsandbox itself, unless
  firebrick configures them in a weaker way than their defaults. Report those to
  the [microsandbox](https://github.com/superradcompany/microsandbox) project.
- What the agent does inside the guest, including becoming root through `sudo`.
- Misuse of access the user granted, such as an allowed host, a mounted
  directory, a secret for its allowed hosts or the shared clipboard.
- Windows, which has no release yet.

When in doubt, report it anyway.
