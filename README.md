# Lux Plugins

This repository is the default plugin store for [Lux](https://github.com/Qoo-330ml/Lux).

This repository contains the plugin source code. A push to `main` starts
`.github/workflows/release.yml`, which builds both `linux-x86_64` and `linux-aarch64` packages on
matching GitHub-hosted runners, publishes them as Release assets, and updates `index.json` with
the Release URLs and SHA-256 digests. Each plugin owns a stable Release whose tag is its plugin
ID. Re-running the workflow uploads the current packages to that plugin's existing Release; a
changed plugin version is added as a new versioned asset in the same Release, while retrying the
same version replaces the same asset. Lux validates the ZIP and its manifest before installing it
into `/config/plugins`.

The package asset name includes the plugin version and target architecture, for example
`org.lux.tmdb-0.1.5-linux-x86_64.zip` and `org.lux.tmdb-0.1.5-linux-aarch64.zip`. The Lux host
selects the matching package from `packages` and stores it under its own canonical plugin ZIP
name after downloading it.

Do not commit credentials, local configuration, media data, or unreviewed executable packages.

## Douban metadata

`org.lux.douban` implements the Lux v1 metadata RPC contract for Douban. It supports Movie and
Series search, metadata, poster images, cast/director credits, the Douban provider ID, and
available trailers. Season metadata is supported when the upstream subject represents a season;
episode, person, and collection metadata are reported as unsupported because the referenced
Douban mobile API does not expose a stable equivalent.

Search uses Douban's public subject-suggest endpoint. Details and richer metadata use the
WeChat-compatible client with the public client credential shipped by the upstream Jellyfin
Douban plugin. No credential configuration is required; the plugin is usable immediately after
installation. The optional `requestIntervalMs` setting only tunes request pacing. For private
testing or a future credential rotation, environment variables can override the built-in client
key without changing the package. Credentials are never included in RPC results or logs. The
plugin applies a bounded response size, HTTPS endpoint validation, rate limiting, retries for
timeouts/429/5xx, and a short-lived bounded response cache.

## Intro/outro detector

`org.lux.intro-outro-detector` implements the Lux v1 `chapter_detector` contract. It receives only
bounded raw Chromaprint point sequences selected by Lux for at least two episodes in one season.
Each Base64 payload is a little-endian sequence of `uint32` fingerprint points; one point represents
`1,238,095` ticks. The detector compares aligned points with a bounded Hamming-distance tolerance,
requires a non-trivial shared sequence, and uses support across the available episodes before
emitting a candidate. It does not invoke ffmpeg, access media paths, open network connections, or
receive source IDs and URLs. It returns only `IntroStart`, `IntroEnd`, and `CreditsStart` candidates;
Lux remains responsible for time-range validation, confidence filtering, persistence, and
Emby-compatible chapter output.

## TheIntroDB online chapter source

`org.lux.theintrodb-chapter-source` is an independent online chapter source. It queries
[TheIntroDB](https://theintrodb.org/) using stored TMDb, TVDb, or IMDb metadata, season/episode numbers,
and optional runtime. It receives no media path, URL, audio fingerprint, or task object, and never runs
ffmpeg or ffprobe. Empty upstream results preserve existing chapters. Its exact boundary and configuration
are documented in `README-theintrodb.md`.
