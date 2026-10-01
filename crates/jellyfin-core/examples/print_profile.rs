//! Dev tool: dump the exact DeviceProfile JSON the app sends, for probing
//! PlaybackInfo behavior against a real server with external tooling.
//!
//!   cargo run -p jellyfin-core --example print_profile [bitrate_cap_bps]
fn main() {
    let cap = std::env::args().nth(1).and_then(|s| s.parse().ok());
    println!(
        "{}",
        serde_json::to_string_pretty(&jellyfin_core::build_device_profile(cap))
            .expect("profile serializes")
    );
}
