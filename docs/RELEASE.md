# Release checklist

Treat source publication and app distribution as separate checks. Passing
unit tests does not establish visual quality, HDR output, notarization or
license completeness of the native libraries in a downloadable bundle.

## Public source tree

- Run `python3 scripts/check-public-tree.py`. It checks local document
  links, local-only files, developer paths, private IP examples and common
  credential formats without printing matched values.
- Run Gitleaks on the exact source snapshot, with `--redact=100`. Review
  tracked images and fixtures too: text scanners do not inspect pixels or
  establish that arbitrary strings are synthetic.
- Keep `internal/`, local notes, signing settings, generated media,
  diagnostic logs, app state and build output out of the publication.
- For a fresh public repository, copy only reviewed source files and
  create fresh Git history. Copying `.git` also copies historical content
  that a clean current tree cannot sanitize.
- Verify the README's repository, issue and release links against the
  destination repository. Enable GitHub private vulnerability reporting
  so the process in `SECURITY.md` is available.

## Code gates

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny --locked check licenses
cargo deny --locked check advisories
```

The advisory command currently fails on unmaintained transitive GPUI
crates. Do not suppress the entire advisory check or interpret its failure
as permission to ignore new vulnerabilities. Record the complete findings,
dependency paths, reachability and release decision for each build.

As of the 2026-09-30 audit, the reported unmaintained crates were
`async-std`, `instant`, `paste`, `proc-macro-error2`, `rustls-pemfile`,
`rustybuzz` and `ttf-parser`; no vulnerability or yanked-version finding
was reported in that run. This is a dated observation, not a permanent
allowlist. The pinned GPUI release has no safe upgrade for those
maintenance advisories. Recheck before every release.

The workspace suite includes headless native-player tests. Live-server and
some corpus-dependent tests are ignored or skip when their inputs are
absent; a green default run does not replace the following checks:

```sh
dev/server.sh corpus
dev/server.sh up
dev/run-server-suite.sh
scripts/bundle-app.sh
```

Also run the live suite against the optional Jellyfin 12 server when
claiming compatibility with that line, using the test environment overrides
in [../dev/README.md](../dev/README.md).

The CI workflow runs only the public-tree check, formatting and a Gitleaks
scan of the pushed source. Clippy, the workspace tests, the live suite and
the bundle run locally before every release.

## App distribution

- Build from the exact reviewed source and workspace version. Preserve the
  source snapshot that corresponds to the binary.
- Require a full bundle E2E pass and verify that all loaded native
  dependencies come from the bundle or macOS system locations.
- Use Developer ID signing, notarization and stapling; archive after
  stapling. See [BUILD.md](BUILD.md).
- Include license texts and copyright notices for the Rust dependency
  closure, embedded fonts and icons, and every bundled native library.
  `THIRD-PARTY.md` is an inventory, not the complete set of notices.
- Provide the corresponding source required by the distributed components'
  licenses, including native-library pins, build scripts and the exact
  Homebrew leaf-library versions/sources. A passing cargo-deny license
  check covers Rust license policy, not these distribution materials.
- Test the quarantined download on a Mac without Homebrew, Xcode or the
  vendor prefix. Validate macOS 13 support on that OS before claiming it.
- Check playback, track selection, subtitles, seeking, resume, account
  switching, miniplayer, media keys, HDR and multiple displays in the real
  app. Test text entry with the intended keyboard layouts; the current
  text field supports append/backspace and does not implement IME.
- Publish the archive checksum and concise release notes with known
  limitations. Updates are manual through the About window's releases link.
