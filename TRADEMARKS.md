# Trademarks and brand assets

The source code in this repository is licensed under the GNU General Public
License, version 3 or later (see `LICENSE`). That licence covers the code. It
does not grant any right to use the project's identity, which the GPL
explicitly allows a licensor to withhold (GPL-3.0 section 7(e)).

The following are reserved and are **not** licensed for use by derived works:

- The name **Jellybeam**, alone or as part of another name.
- The Jellybeam wordmark and its rays.
- The Jellybeam mascot in every pose and tier, including the app icon and
  the empty-state, pairing and sidebar artwork (`crates/app/assets/brand/jellybeam/`,
  `docs/brand/`, and the icon the bundle script builds from them).
- The bundle identifier `tv.jellybeam.Jellybeam`, which identifies this app
  to macOS and to Jellyfin servers, and the Keychain service name
  `tv.jellybeam.jellyfin`.

## What this means for a fork

You may copy, modify, build, and redistribute the code under the GPL,
including for a fee, provided you meet the GPL's terms. A build you
distribute that is not this project's own release must:

- use a different name and bundle identifier (`scripts/Info.plist.in`);
- replace the wordmark, mascot and app icon (the files under
  `crates/app/assets/brand/` listed in `docs/DESIGN-GUIDE.md Part A`) with its own;
- not present itself as Jellybeam, as endorsed by Jellybeam, or as an
  official build.

Describing a fork factually as "based on Jellybeam" is fine, as is keeping
the attribution the GPL requires.

## Jellyfin

Jellyfin is a trademark of the Jellyfin project. Jellybeam is an independent
client and is not affiliated with or endorsed by the Jellyfin project.
