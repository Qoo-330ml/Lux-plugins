# Lux Plugins

This repository is the default plugin store for [Lux](https://github.com/Qoo-330ml/Lux).

`index.json` is the store catalog. Each catalog entry points to a versioned plugin ZIP and
includes its SHA-256 digest. Lux validates the ZIP and its manifest before installing it into
`/config/plugins`.

The packages in this catalog are built for `linux-x86_64`, matching the first supported NAS
deployment target. Add platform-specific packages and entries as they become available for other
deployment targets.

Do not commit credentials, local configuration, media data, or unreviewed executable packages.
