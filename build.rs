fn main() {
    // CPython resolves Python symbols when loading the macOS shared module.
    #[cfg(feature = "python")]
    pyo3_build_config::add_extension_module_link_args();
}
