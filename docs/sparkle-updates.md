# macOS app updates

The notarized `devserial.app` supports macOS 12 or newer and embeds Sparkle
2.10.0. The CLI archives and Homebrew formula do not contain Sparkle.

The app's `SUFeedURL` is `https://devserial.metaneutrons.cc/appcast.xml`.
Cloudflare serves it from the `devserial-updates` R2 bucket. The release job
downloads a SHA-256-pinned Sparkle distribution, embeds the framework, signs
its nested helpers and the outer app, then notarizes and staples the bundle.
The update payload is the exact notarized `.app.zip` also staged on GitHub.

`publish-sparkle` runs after GitHub staging and the Homebrew and AUR channels,
before the release is promoted to Latest. It signs that ZIP with the dedicated
Sparkle Ed25519 key, verifies the signature against `SUPublicEDKey` in the
bundle, and rejects a corrupted-byte probe. It writes the ZIP under an
immutable name, reads back authenticated and public bytes, and then updates
the append-only appcast with an R2 conditional write. Re-running an identical
release is idempotent; a conflicting or older release is rejected. A failed
Sparkle publication prevents Latest promotion.

The protected `release-sparkle` GitHub environment accepts only
`devserial-v*` tags. It contains `SPARKLE_ED_PRIVATE_KEY` (the original
Sparkle-exported base64 seed), plus `R2_ACCESS_KEY_ID` and
`R2_SECRET_ACCESS_KEY` from a token scoped to this bucket. These values are
never committed. The private Sparkle key is backed up offline; retain it for
all future updates of installed versions. Rotating the key requires a
separately qualified transition release.

The feed is first created with the first Sparkle-enabled stable release; an
empty bucket therefore returns HTTP 404 before that release. Do not publish a
manual feed entry or replace a signed archive to recover a failed release.
Repair the cause and rerun the same immutable release workflow.
