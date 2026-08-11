# Lux Plugins

This repository is the default plugin store for [Lux](https://github.com/Qoo-330ml/Lux).

This repository contains the plugin source code. A push to `main` starts
`.github/workflows/release.yml`, which builds both `linux-x86_64` and `linux-aarch64` packages on
matching GitHub-hosted runners, publishes them as Release assets, and updates `index.json` with
the Release URLs and SHA-256 digests. Lux validates the ZIP and its manifest before installing it
into `/config/plugins`.

The package asset name includes the plugin version and target architecture, for example
`org.lux.tmdb-0.1.5-linux-x86_64.zip` and `org.lux.tmdb-0.1.5-linux-aarch64.zip`. The Lux host
selects the matching package from `packages` and stores it under its own canonical plugin ZIP
name after downloading it.

Do not commit credentials, local configuration, media data, or unreviewed executable packages.
