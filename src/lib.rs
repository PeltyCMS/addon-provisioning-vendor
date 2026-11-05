//! Vendor reseller provisioning addon (MarketConnect-style resale).
//! The whole module lives in [`vendor`]; this file only exports it to the host.
pub mod vendor;

#[cfg(all(target_family = "wasm", feature = "export"))]
pelty_addon_sdk::provisioning_addon!(vendor::VendorReseller);
