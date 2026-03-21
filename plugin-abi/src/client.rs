/// Generate the boilerplate for a CLA plugin entry point.
///
/// This macro emits:
/// - `HARDY_ABI_TOKEN` — the static ABI version token
/// - `hardy_create_cla` — the `extern "C"` factory function with panic
///   guard, config parsing, and error handling
///
/// The closure receives the deserialized config and must return
/// `Result<Arc<dyn hardy_bpa::cla::Cla>, String>`.
///
/// # Example
///
/// ```ignore
/// hardy_plugin_abi::export_cla!(config::Config, |config| {
///     Ok(Arc::new(MyCla::new(config)))
/// });
/// ```
#[macro_export]
macro_rules! export_cla {
    ($config_type:ty, $factory:expr) => {
        #[unsafe(no_mangle)]
        pub static HARDY_ABI_TOKEN: &str = $crate::ABI_TOKEN;

        #[unsafe(no_mangle)]
        pub extern "C" fn hardy_create_cla(
            config_json: *const ::std::ffi::c_char,
        ) -> $crate::PluginResult<::std::sync::Arc<dyn ::hardy_bpa::cla::Cla>> {
            $crate::guard_factory(|| {
                let config: $config_type =
                    unsafe { $crate::parse_config(config_json) }.map_err(|e| {
                        ::tracing::error!("Plugin config error: {e}");
                        -1
                    })?;
                let factory: fn(
                    $config_type,
                ) -> ::std::result::Result<
                    ::std::sync::Arc<dyn ::hardy_bpa::cla::Cla>,
                    ::std::string::String,
                > = $factory;
                factory(config).map_err(|e| {
                    ::tracing::error!("Plugin creation error: {e}");
                    -2
                })
            })
        }
    };
}
