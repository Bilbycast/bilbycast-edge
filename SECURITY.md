# Security Policy

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

If you believe you have found a security vulnerability in any bilbycast project, please report it privately using one of the following channels:

- **Email:** admin@softsidetech.com
- **GitHub:** Use [Private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability) via the repository's **Security** tab.

Please include:

- A description of the vulnerability and its potential impact
- Steps to reproduce (proof-of-concept code, configs, or packet captures where applicable)
- The affected version(s), commit hash, or release tag
- Any suggested remediation you have in mind

## What to Expect

- **Acknowledgement:** within 3 business days
- **Initial assessment:** within 10 business days
- **Fix or mitigation timeline:** communicated after triage; critical issues prioritised
- **Coordinated disclosure:** we will work with you on a disclosure timeline and credit you in the advisory unless you prefer to remain anonymous

## Scope

In scope:

- All bilbycast projects published under this organisation (edge, manager, relay, srt, rist, fdk-aac-rs, ffmpeg-video-rs, libsrt-rs, appear-x-api-gateway)
- Authentication, authorisation, crypto, transport security, and data-path integrity issues
- Dependency vulnerabilities with a realistic exploit path against bilbycast

Out of scope:

- Vulnerabilities in third-party services or infrastructure we do not operate
- Social engineering, physical attacks, or denial-of-service without a novel amplification vector
- Missing defence-in-depth hardening that is not independently exploitable

## Supported Versions

Security fixes are applied to the latest release and, where practical, backported to the immediately preceding minor release. Older versions are not supported — please upgrade.

## Safe Harbour

We will not pursue legal action against researchers who:

- Make a good-faith effort to avoid privacy violations, data destruction, or service disruption
- Report the issue privately via the channels above
- Give us reasonable time to remediate before any public disclosure
