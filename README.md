# Lux Plugins

This repository is the default plugin store for [Lux](https://github.com/Qoo-330ml/Lux).

This repository contains the plugin source code. A push to `main` starts
`.github/workflows/release.yml`, which builds both `linux-x86_64` and `linux-aarch64` packages on
matching GitHub-hosted runners, publishes them as Release assets, and updates `index.json` with
the Release URLs and SHA-256 digests. Lux validates the ZIP and its manifest before installing it
into `/config/plugins`.

The package asset name includes the plugin version and target architecture, for example
`org.lux.tmdb-0.1.6-linux-x86_64.zip` and `org.lux.tmdb-0.1.6-linux-aarch64.zip`. The Lux host
selects the matching package from `packages` and stores it under its own canonical plugin ZIP
name after downloading it.

Do not commit credentials, local configuration, media data, or unreviewed executable packages.

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
