# Changelog

## [0.1.7](https://github.com/metaneutrons/devserial/compare/devserial-v0.1.6...devserial-v0.1.7) (2026-09-05)


### Bug Fixes

* **ci:** kein Debug-Paket aus dem AUR-Quellpaket, und eindeutige Auswahl ([#32](https://github.com/metaneutrons/devserial/issues/32)) ([b43a93a](https://github.com/metaneutrons/devserial/commit/b43a93a4da482576d3661a4afade3e3f7cd02544))

## [0.1.6](https://github.com/metaneutrons/devserial/compare/devserial-v0.1.5...devserial-v0.1.6) (2026-09-05)


### Bug Fixes

* **ci:** der AUR-Preflight wies eine gueltige Credential ab ([#30](https://github.com/metaneutrons/devserial/issues/30)) ([ee29580](https://github.com/metaneutrons/devserial/commit/ee295801cc2b2f6c2eec5873aaecbab180376a2c))

## [0.1.5](https://github.com/metaneutrons/devserial/compare/devserial-v0.1.4...devserial-v0.1.5) (2026-09-05)


### Features

* rework the architecture, apply the repository standard and harden the release ([#6](https://github.com/metaneutrons/devserial/issues/6)) ([578da88](https://github.com/metaneutrons/devserial/commit/578da88b9d81ebe023f7e49178e1b779d5768dbd))


### Bug Fixes

* **ci:** accept both spellings file uses for a statically linked binary ([#16](https://github.com/metaneutrons/devserial/issues/16)) ([fbeb134](https://github.com/metaneutrons/devserial/commit/fbeb134dea72fdd9b3ad65773d7a647271d2f0c8))
* **ci:** das App-Token ueber client-id statt app-id ausstellen ([#29](https://github.com/metaneutrons/devserial/issues/29)) ([4c969ea](https://github.com/metaneutrons/devserial/commit/4c969ea49b96a27077819d1aafeb667456a5eed1))
* **ci:** put the AUR licences under $pkgname and disable LTO for the source build ([#17](https://github.com/metaneutrons/devserial/issues/17)) ([38fa49e](https://github.com/metaneutrons/devserial/commit/38fa49e661528ef48f90f5384234a71fc0b841b3))
* **ci:** read the release state back without the field that does not exist ([#18](https://github.com/metaneutrons/devserial/issues/18)) ([bbea268](https://github.com/metaneutrons/devserial/commit/bbea268e84e20d178533e039b99099421c7fb96e))

## [0.1.4](https://github.com/metaneutrons/devserial-mcp/compare/devserial-v0.1.3...devserial-v0.1.4) (2026-08-23)


### Bug Fixes

* **ci:** disable monitor feature for aarch64-linux targets in release workflow ([c1ab527](https://github.com/metaneutrons/devserial-mcp/commit/c1ab5274efc091685eb4d9f3765fccae2f16f26f))
* **ci:** disable monitor feature for musl targets in release workflow ([bf19f9d](https://github.com/metaneutrons/devserial-mcp/commit/bf19f9da4669a43ce0243e6b794018a54a8d3009))
* **ci:** use full release tag in homebrew formula download URL ([3d2aae0](https://github.com/metaneutrons/devserial-mcp/commit/3d2aae0adb55d3fcb4d03d9b5816b22f6b5ed86e))
* **gui:** enable x11 and wayland features for eframe on Linux ([d3e8851](https://github.com/metaneutrons/devserial-mcp/commit/d3e885140fb317b14ca27934724cd7f0eb1d2f61))
* **ipc:** support non-unix targets and gate daemon lifecycle tests ([8936247](https://github.com/metaneutrons/devserial-mcp/commit/8936247a4011afa92ca0a506e88f50909e6c9365))

## [0.1.3](https://github.com/metaneutrons/devserial-mcp/compare/devserial-v0.1.2...devserial-v0.1.3) (2026-08-23)


### Features

* add background daemon, modular CLI, serial BREAK, and X/Y/ZMODEM protocols ([#3](https://github.com/metaneutrons/devserial-mcp/issues/3)) ([010f7a2](https://github.com/metaneutrons/devserial-mcp/commit/010f7a272800cec71d8401396cfc5c859532c378))

## [0.1.2](https://github.com/metaneutrons/devserial-mcp/compare/devserial-v0.1.1...devserial-v0.1.2) (2026-05-25)


### Bug Fixes

* **ci:** disable monitor feature for Linux ARM64 cross-compile ([0f957f4](https://github.com/metaneutrons/devserial-mcp/commit/0f957f44a01aed9f22d79453c9d2560315fa7a0b))
* **ci:** use native ARM64 runners instead of cross-compile ([518a899](https://github.com/metaneutrons/devserial-mcp/commit/518a8990d5904653a4d8d360924721aedcd70bcf))

## [0.1.1](https://github.com/metaneutrons/devserial-mcp/compare/devserial-v0.1.0...devserial-v0.1.1) (2026-05-25)


### Features

* **cli:** show help when run interactively, MCP only via pipe ([9cec90a](https://github.com/metaneutrons/devserial-mcp/commit/9cec90a169db895e18dcbd36f7b4be842a8933dd))
* initial release ([b76b5c5](https://github.com/metaneutrons/devserial-mcp/commit/b76b5c5b8c098d6019f6dfb9e48862a800ef82a2))
