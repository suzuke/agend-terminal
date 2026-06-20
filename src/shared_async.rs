//! Shared tokio-runtime construction for the sync→async channel bridges.

/// #2050 simplify PR-E (⑬): build the `current_thread` tokio runtime used by the
/// per-channel sync→async bridges (telegram / discord). The construction template
/// `new_current_thread().enable_all().build()` was duplicated verbatim in both
/// runtime getters, differing only by the `expect` label. Runtime PARAMETERS are
/// unchanged — this only dedups the builder boilerplate.
pub(crate) fn build_current_thread_runtime(label: &str) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect(label)
}
