# Spec-version vs. server-version drift policy

`jellyfin-api`'s models are generated (see `regen.sh`) from a **checked-in,
hash-pinned** OpenAPI spec snapshot: `jellyfin-openapi-stable.json`, spec
version **12.0.0**.

The dev/test server this crate is validated against day-to-day
(`dev/docker-compose.yml`) runs Jellyfin **10.11.10** — an older server than
the spec the models were generated from. This is a **deliberate, known
mismatch**, not an oversight:

- There is no Jellyfin-published per-server-version OpenAPI spec URL to pin
  to instead. `api.jellyfin.org/openapi/` only serves `stable` / `unstable`
  (each with one `_previous` snapshot) — floating channel names, not
  version-addressed files like `jellyfin-openapi-10.11.10.json`. The
  `jellyfin/jellyfin` GitHub releases don't attach an `openapi.json` asset
  per tag either. So "pin the spec to exactly 10.11.10" isn't a URL you can
  fetch; the closest available approximation is "pin to a specific spec
  *snapshot*, chosen and hashed deliberately" — which is what `regen.sh`
  does now (see its header comment for the mechanics).
- A newer spec generating the client is the safe direction for drift: newer
  Jellyfin API versions overwhelmingly *add* optional fields and enum
  values rather than remove or change the meaning of existing ones. typify
  generates every field as `Option<T>` unless the schema marks it required,
  and `extract_subset.py` strips `additionalProperties: false` everywhere
  (see its docstring) so responses from an *older* server that's missing
  fields the 12.0.0 spec thinks exist just deserialize those fields as
  `None` — not an error.
- The `#[serde(other)]` fallback injected into every generated
  string enum by `postprocess_enums.py` covers the complementary
  direction: an *older* server sending an enum value the pinned spec
  doesn't know about (rare, but possible if 12.0.0 renamed something)
  deserializes to `Unrecognized` instead of failing the whole page.

Together, those two properties are why "newer spec, older pinned test
server" is safe enough to commit to rather than chase: the client tolerates
both "server is missing a field/value the spec added" and "server sends a
value the spec renamed/dropped."

## What actually catches drift

This policy is a bet, not a proof — it needs a live alarm, not just
reasoning about schema shapes. That alarm is the API contract suite:
`jellyfin-api` + `jellyfin-core`'s contract tests (`dev/run-server-suite.sh`)
run against the live dockerized dev server today, and the opt-in second
server (`dev/server.sh 12-up`, see `dev/README.md`) validates against the next
major Jellyfin line ahead of time, to get early warning if a server ships
something the pinned 12.0.0 spec truly can't tolerate — a required field
that goes missing, a wire shape change deeper than "enum got a new value" or
"object got a new optional field," etc. If a contract-suite run against
either server ever fails, that is the signal to re-pin (see
`regen.sh --update-pin`) and regenerate, not this document.

## Re-pinning

Pinning is deliberate by design (see `regen.sh`'s header comment for the
full flow): `./regen.sh --update-pin` fetches the current `stable` spec and
reports its hash; a human then edits `PINNED_SPEC_SHA256` in `regen.sh` and
re-runs `./regen.sh` normally, reviews the `models.rs` diff, and commits
spec file + pin + regenerated models together. Plain `./regen.sh` never
fetches anything and fails loudly if the checked-in spec file's hash
doesn't match the pin.
