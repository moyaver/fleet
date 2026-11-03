# Security policy

## Reporting a vulnerability

**Don't open a public issue.** Report it privately through GitHub Security Advisories:

1. Go to the repository's **Security** tab.
2. Click **Report a vulnerability**.
3. Describe the issue, the affected version or commit, and how to reproduce it.

You should get an acknowledgement within 7 days. Please give us a reasonable time to fix the issue before you disclose it.

## Scope

Especially welcome: anything that breaks an invariant in [PLAN §3](docs/PLAN.md#3-invariants-non-negotiable), for example:

- a secret leaving a runner unredacted (I6);
- the agent changing Maestro's exit status or output (I1);
- uploaded HTML or log content running script on the app origin (I7);
- authentication or authorization bypass between projects, agents and browsers.

## Supported versions

There is no release yet. Once 1.0.0 ships, the latest minor release gets security fixes.
