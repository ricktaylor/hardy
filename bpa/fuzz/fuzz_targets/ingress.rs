#![no_main]

use hardy_bpa::*;
use libfuzzer_sys::fuzz_target;

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
static INGRESS: std::sync::OnceLock<ingress::Ingress> = std::sync::OnceLock::new();

fn setup() -> tokio::runtime::Runtime {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.spawn(async {
        let mut filename = std::env::current_dir().unwrap();
        filename.push("fuzz/ingress.config");

        let config = config::Config::builder()
            .add_source(config::File::from(filename).format(config::FileFormat::Toml))
            .build()
            .unwrap();
        utils::logger::init(&config);

        // Get administrative endpoints
        let administrative_endpoints = utils::admin_endpoints::AdminEndpoints::init(&config);

        // New store
        let store = store::Store::new();

        // New FIB
        let fib = fib::Fib::new(&config);

        // New registries
        let cla_registry = cla_registry::ClaRegistry::new(&config, fib.clone());
        let app_registry =
            app_registry::AppRegistry::new(&config, administrative_endpoints.clone());

        // Prepare for graceful shutdown
        let (mut task_set, cancel_token) = utils::cancel::new_cancellable_set();

        // Create a new dispatcher
        let dispatcher = dispatcher::Dispatcher::new(
            &config,
            administrative_endpoints,
            store.clone(),
            cla_registry.clone(),
            app_registry.clone(),
            fib.clone(),
            &mut task_set,
            cancel_token.clone(),
        );

        // Create a new ingress
        INGRESS.get_or_init(|| {
            ingress::Ingress::new(
                &config,
                store.clone(),
                dispatcher.clone(),
                &mut task_set,
                cancel_token.clone(),
            )
        });

        while task_set.join_next().await.is_some() {}
    });

    rt
}

fuzz_target!(|data: &[u8]| {
    RT.get_or_init(setup).block_on(async {
        let mut i = 0;
        let ingress = loop {
            match INGRESS.get() {
                Some(ingress) => break ingress,
                None if i < 10 => {
                    i += 1;
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
                None => panic!("Failed to connect to ingress"),
            }
        };
        let _ = ingress.receive(data.into()).await;
    })
});

// llvm-cov show --format=html  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -o ./fuzz/coverage/ingress/ -ignore-filename-regex='/.cargo/|rustc/|/target/'