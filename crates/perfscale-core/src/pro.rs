//! Pro module registry — bake-time advertisement for paid feature families.
//!
//! The engine is open source; paid families (FIX, SOAP, WebRTC, …) live in
//! closed crates and plug into the engine's runtime seams (see
//! [`crate::step::actions::register_action`]). This module adds the missing
//! *discovery* half: every pro crate's `register()` also calls
//! [`register_pro_module`] with a static [`ProModuleInfo`], so the host
//! binary can answer "which pro modules are baked into this build?" at
//! runtime — e.g. the paid agent reports them in its controlplane heartbeat,
//! and the controlplane cross-checks a run config against the target
//! machine's baked set before dispatch.
//!
//! The single source of truth for the catalog data is the lightweight
//! `perfscale-pro-catalog` crate (controlplane repo) — it carries one
//! `ProModuleInfo` const per family and is shared by the pro crates
//! (self-registration), the controlplane BFF (entitlement + validation),
//! and nothing heavy: no media encoders or protocol stacks leak into
//! consumers that only need the metadata.

use std::sync::{OnceLock, RwLock};

/// Static description of one baked-in pro module.
///
/// All fields are `'static` so modules can be declared as `const` items in a
/// catalog crate and passed around by value for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProModuleInfo {
    /// Module name without the `pro/` prefix, e.g. `"webrtc"`.
    pub name: &'static str,
    /// Action IDs this module serves on the action seam, e.g.
    /// `&["pro/webrtc-connect@v1", …]`. Empty for modules that only extend
    /// existing `std/*` actions (observers, drivers).
    pub actions: &'static [&'static str],
    /// Metrics family registered with [`crate::metrics::register_family`],
    /// e.g. `Some("pro/webrtc")`. `None` when the module's metrics are
    /// emitted by the engine itself (e.g. the pubsub drivers).
    pub metrics_family: Option<&'static str>,
    /// Top-level config block this module unlocks (e.g. `Some("webrtc")` for
    /// the `webrtc:` run-config block), if any.
    pub config_block: Option<&'static str>,
    /// One-line human description for catalogs and logs.
    pub description: &'static str,
}

fn module_registry() -> &'static RwLock<Vec<ProModuleInfo>> {
    static REGISTRY: OnceLock<RwLock<Vec<ProModuleInfo>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a baked-in pro module. Idempotent by `name`: a repeat call
/// replaces the previous entry (a crate's `register()` may run more than
/// once — e.g. under `Once` in tests and once in `main`).
pub fn register_pro_module(info: ProModuleInfo) {
    let mut modules = module_registry().write().unwrap();
    match modules.iter_mut().find(|m| m.name == info.name) {
        Some(slot) => *slot = info,
        None => modules.push(info),
    }
}

/// Every pro module registered so far, in registration order.
pub fn pro_modules() -> Vec<ProModuleInfo> {
    module_registry().read().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MODULE: ProModuleInfo = ProModuleInfo {
        name: "test-widget",
        actions: &["pro/test-widget@v1"],
        metrics_family: Some("pro/test-widget"),
        config_block: None,
        description: "test module",
    };

    #[test]
    fn registers_and_lists_modules() {
        register_pro_module(TEST_MODULE);
        assert!(pro_modules().contains(&TEST_MODULE));
    }

    #[test]
    fn repeat_registration_replaces_instead_of_duplicating() {
        register_pro_module(TEST_MODULE);
        register_pro_module(TEST_MODULE);
        let count = pro_modules()
            .iter()
            .filter(|m| m.name == "test-widget")
            .count();
        assert_eq!(count, 1);
    }
}
