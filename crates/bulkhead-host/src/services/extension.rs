//! Built-in packages apps call by name: `capabilities.extensions."<package>"`.
//!
//! An app declares a package with a scope table, the operator grants it by
//! package name in the policy's `grant` lists, and the app calls its
//! functions through the node (`Request::Extension`,
//! `bulkhead_sdk::extension::call`). The one package is Modbus's
//! (`bulkhead-modbus:modbus`, [`crate::services::modbus`]): the node checks
//! each call and forwards it to the Modbus drivers. A call of any other
//! package answers `NoSuchExtension`.
//!
//! Until ROADMAP DA8 (2026-11-15) a package could also be served by an
//! `Extension`, host code linked into the node from outside this crate. No
//! third-party code runs in the node any more: a service of its own is an
//! app that others call under messaging ACLs (`examples/greeter`).

/// What the manifest asks of one package for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExtensionSpec {
    /// The `capabilities.extensions."<package>"` table.
    pub scope: serde_json::Value,
    /// Whether the operator granted it. Declared packages start granted;
    /// narrow with [`InstanceSpec::with_granted_extensions`](crate::InstanceSpec::with_granted_extensions).
    pub granted: bool,
}

impl ExtensionSpec {
    /// A declared package with `scope`, granted.
    #[must_use]
    pub fn new(scope: serde_json::Value) -> Self {
        Self {
            scope,
            granted: true,
        }
    }
}
