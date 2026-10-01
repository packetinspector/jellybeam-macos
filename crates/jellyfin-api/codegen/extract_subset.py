#!/usr/bin/env python3
"""Extract the transitive closure of schemas needed by `jellyfin-api`'s
frozen interface (see ../src/lib.rs) from the Jellyfin OpenAPI doc into a
standalone JSON Schema document that `cargo-typify` can consume (rewrites
'#/components/schemas/X' refs to '#/$defs/X').

ROOTS covers everything the client's frozen interface needs: full
BaseItemDto browse fields (pulled automatically, it's a root type),
MediaStream, the DeviceProfile family (DirectPlayProfile/TranscodingProfile/
SubtitleProfile/CodecProfile all come in transitively via DeviceProfile
refs), UserItemDataDto, QuickConnectResult, trickplay metadata
(TrickplayInfoDto), the WebSocket event payload shapes (LibraryUpdateInfo,
UserDataChangeInfo), and MediaSegmentDtoQueryResult (GET
/MediaSegments/{itemId} response wrapper, which pulls in MediaSegmentDto ->
MediaSegmentType transitively).

additionalProperties:false stripping (the load-bearing fix, see below) is a
hard requirement.

This keeps the generated crate small while still trivially adjustable: add
a type to ROOTS and rerun to pull in more of the API surface, or pass
--full to skip filtering and emit all schemas.
"""
import argparse
import json

SRC = "jellyfin-openapi-stable.json"

ROOTS = [
    "BaseItemDto",
    "BaseItemDtoQueryResult",  # wraps GET /Items, /UserViews, /UserItems/Resume, /Shows/NextUp
    "AuthenticationResult",
    "AuthenticateUserByName",  # POST /Users/AuthenticateByName request body
    "UserDto",
    "SessionInfoDto",
    "PlaybackInfoDto",  # POST /Items/{itemId}/PlaybackInfo request body
    "PlaybackInfoResponse",
    "MediaSourceInfo",
    "MediaStream",
    "DeviceProfile",  # pulls DirectPlayProfile/TranscodingProfile/SubtitleProfile/CodecProfile
    "QuickConnectResult",
    "QuickConnectDto",
    "UserItemDataDto",
    "TrickplayInfoDto",
    "LibraryUpdateInfo",  # WebSocket LibraryChanged event payload
    "UserDataChangeInfo",  # WebSocket UserDataChanged event payload
    "MediaSegmentDtoQueryResult",  # wraps GET /MediaSegments/{itemId}; pulls in MediaSegmentDto -> MediaSegmentType
]


def find_refs(node, out):
    if isinstance(node, dict):
        for k, v in node.items():
            if k == "$ref" and isinstance(v, str) and v.startswith("#/components/schemas/"):
                out.add(v.split("/")[-1])
            else:
                find_refs(v, out)
    elif isinstance(node, list):
        for x in node:
            find_refs(x, out)


def rewrite(node):
    """Rewrite $ref targets for the standalone doc, and drop
    'additionalProperties: false'.

    Jellyfin's spec marks almost every object schema 'additionalProperties:
    false'. typify honors that literally and emits
    '#[serde(deny_unknown_fields)]', which is the OPPOSITE of what we need:
    a hard requirement here is tolerating unknown fields from server drift
    (older/newer Jellyfin servers, custom builds, future spec additions).
    So this generator intentionally treats the spec's 'closed object' intent
    as advisory only and always allows unknown fields at deserialization
    time.
    """
    if isinstance(node, dict):
        out = {}
        for k, v in node.items():
            if k == "$ref" and isinstance(v, str) and v.startswith("#/components/schemas/"):
                out[k] = v.replace("#/components/schemas/", "#/$defs/")
            elif k == "additionalProperties" and v is False:
                continue  # drop it; absence means "unknown fields allowed"
            else:
                out[k] = rewrite(v)
        return out
    elif isinstance(node, list):
        return [rewrite(x) for x in node]
    else:
        return node


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--full", action="store_true", help="emit all schemas, no filtering")
    ap.add_argument("-o", "--output", default="jellyfin-schemas-subset.json")
    args = ap.parse_args()

    with open(SRC) as f:
        spec = json.load(f)
    schemas = spec["components"]["schemas"]

    if args.full:
        selected = schemas
    else:
        visited = set()
        stack = list(ROOTS)
        missing = []
        while stack:
            name = stack.pop()
            if name in visited:
                continue
            visited.add(name)
            s = schemas.get(name)
            if s is None:
                missing.append(name)
                continue
            refs = set()
            find_refs(s, refs)
            for r in refs:
                if r not in visited:
                    stack.append(r)
        if missing:
            print("WARNING: root types not found in spec:", missing)
        selected = {k: schemas[k] for k in visited if k in schemas}

    rewritten = rewrite(selected)
    doc = {
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "JellyfinSchemas",
        "$defs": rewritten,
    }
    with open(args.output, "w") as f:
        json.dump(doc, f, indent=2)
    print(f"Wrote {args.output} with {len(rewritten)} schemas (of {len(schemas)} total)")


if __name__ == "__main__":
    main()
