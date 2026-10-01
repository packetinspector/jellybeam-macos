//! Fuzz target: server-JSON deserialization into the hot DTOs.
//!
//! The server is untrusted; every byte sequence must either parse or fail
//! cleanly -- any panic/OOM/stack-overflow here is a finding. Seeds: the
//! crate's own tests/fixtures/*.json (wired in by scripts, see
//! REPRO/README-fuzz.md).

#![no_main]

use libfuzzer_sys::fuzz_target;

use jellyfin_api::models::{
    AuthenticationResult, BaseItemDto, MediaSegmentDtoQueryResult, PlaybackInfoResponse,
    TrickplayInfoDto,
};

fuzz_target!(|data: &[u8]| {
    // Raw bytes -> each hot DTO. from_slice exercises UTF-8 handling too.
    let _ = serde_json::from_slice::<BaseItemDto>(data);
    let _ = serde_json::from_slice::<PlaybackInfoResponse>(data);
    let _ = serde_json::from_slice::<AuthenticationResult>(data);
    let _ = serde_json::from_slice::<MediaSegmentDtoQueryResult>(data);
    let _ = serde_json::from_slice::<TrickplayInfoDto>(data);
    // The list shape the sync path actually pulls (items arrays).
    let _ = serde_json::from_slice::<Vec<BaseItemDto>>(data);
});
