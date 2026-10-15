# Security policy

Ferry runs commands over SSH on machines you configure, as your user. It is
not a sandbox (see the README's Limits), so a recipe can do anything you can.

## Reporting a vulnerability

Please report a vulnerability privately, through GitHub's private
vulnerability reporting: on the repository's **Security** tab, choose
**Report a vulnerability**. Do not open a public issue for it.

Include the Ferry version on each end (`ferry --version`), what you ran, and
what an attacker could do. You will get a reply within a week. A fix is
released before the details are made public.

## Supported versions

Ferry is pre-1.0: only the latest release receives fixes.
