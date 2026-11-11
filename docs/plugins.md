<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Plugins

A plugin is a separate process that works like a pure function: it receives meeting data (the transcript with word
ids, times and speakers) and returns values for the app to display, such as a summary, action items or talk time.
The manifest format is in [manifest-draft.md](manifest-draft.md) (Korean, a draft).

## Where plugins live

- **The catalog** is [Eastsidegunn/meetko-plugins](https://github.com/Eastsidegunn/meetko-plugins). Its `index.json`
  feeds the app's **Browse** list.
- **Official examples** are subfolders of that repository: `summary-py`, `action-items-py`, and `talk-time-rs`, a
  source-only Rust example that is not in the Browse list.
- **Third-party plugins** live in their authors' own repositories and are listed by a pull request to meetko-plugins.
- **Plugin ids** are `owner/name`, for example `eastsidegunn/summary-py`. The first part must match the repository
  owner.

This repository keeps only test fixtures (`tests/fixtures/plugins/`).

## Security

Only install plugins you trust. Details and known gaps are in [plugin-security.md](plugin-security.md).

**macOS.** Plugins run inside an OS sandbox (`sandbox-exec`). It blocks:

- the network, unless the manifest declares `network = true` and you agreed at install time (even then IP only, no
  Unix sockets);
- your home folder and all of `/Users`;
- the list, command lines and environment of other processes;
- starting other programs;
- the meetko data folder (recordings, database, other meetings, settings);
- other temporary files (`/private/tmp` and the like) and external drives (`/Volumes`);
- most writes, including to the plugin's own install folder. The only writable place is a temporary HOME created fresh
  for each run.

If the sandbox cannot be set up, the plugin does not run. A manifest's `python3` always runs as Apple's
`/usr/bin/python3` regardless of `PATH` (Homebrew Python and Python installed directly under the home folder are
refused), so the Xcode Command Line Tools (`xcode-select --install`) are required. For development only, the sandbox
can be turned off with `MEETKO_PLUGIN_SANDBOX=off`; the result is then recorded as run without isolation.

**What it does not block.** A plugin still runs on your computer. CPU and memory are not limited (there is only a time
limit). The profile is deny-by-default and allows only what is listed, so anything not named above is blocked too.
A plugin that declared network access can send to any host, so it can send out what it reads (the meeting transcript
it was given). Audio is never passed to plugins.

**Windows and Linux.** There is no isolation yet. The manifest's network and audio permissions are declarations only;
plugins run with your user account's permissions and can read whatever files your account can read and use the
network.

**API keys.** A plugin that calls a hosted AI service declares the key names it needs; you enter the values, which are
stored in the macOS keychain only and handed to that plugin per run over stdin (no secret store on other systems yet).
See [plugin-security.md](plugin-security.md#secrets-api-keys-and-settings).

## Installing

```sh
meetko plugin info    github:OWNER/REPO@REF            # download and show the manifest and permissions; does not install
meetko plugin install github:OWNER/REPO@REF            # show permissions, ask for consent, then install
meetko plugin install github:OWNER/REPO/SUB/PATH@REF   # a plugin in a subfolder, e.g. an official example
meetko plugin install ./my-folder                      # a local folder works too (same as plugin add)
```

In the app, **Browse** on the plugin screen lists the plugins on GitHub and installs them; it connects to the internet
only when you press the button. Being listed means only that a plugin passed the format checks; **nobody reviews the
code**. An install resolves the tag and **pins the exact commit**, and nothing is built or run during the install:
the downloaded files are used as they are.

## Publishing

Put the plugin in your own public GitHub repository with its `meetko.toml`, tag a release, and open a pull request to
[meetko-plugins](https://github.com/Eastsidegunn/meetko-plugins) to get listed. Manifest rules and the checks an
install runs are in [plugin-publishing.md](plugin-publishing.md) (Korean, with an English summary).
