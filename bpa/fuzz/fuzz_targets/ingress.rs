#![no_main]

use hardy_bpa::*;
use libfuzzer_sys::fuzz_target;

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
static DISPATCHER: std::sync::OnceLock<std::sync::Arc<dispatcher::Dispatcher>> =
    std::sync::OnceLock::new();

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
        let store = store::Store::new(&config, false);

        // New FIB
        let fib = fib::Fib::new(&config);

        // New registries
        let cla_registry = cla_registry::ClaRegistry::new(&config, fib.clone());
        let app_registry =
            app_registry::AppRegistry::new(&config, administrative_endpoints.clone());

        // Prepare for graceful shutdown
        let (mut task_set, cancel_token) = utils::cancel::new_cancellable_set();

        // Load static routes
        if let Some(fib) = &fib {
            static_routes::init(&config, fib.clone(), &mut task_set, cancel_token.clone()).await;
        }

        // Create a new dispatcher
        let dispatcher = dispatcher::Dispatcher::new(
            &config,
            administrative_endpoints,
            store.clone(),
            cla_registry,
            app_registry,
            fib,
            &mut task_set,
            cancel_token.clone(),
        );

        // Start the store - this can take a while as the store is walked
        store
            .start(dispatcher.clone(), &mut task_set, cancel_token.clone())
            .await;

        DISPATCHER.get_or_init(|| dispatcher);

        while task_set.join_next().await.is_some() {}
    });

    rt
}

fn test_ingress(data: &[u8]) {
    RT.get_or_init(setup).block_on(async {
        let dispatcher = loop {
            match DISPATCHER.get() {
                Some(dispatcher) => break dispatcher,
                None => {
                    tokio::task::yield_now().await;
                }
            }
        };

        let metrics = RT.get().unwrap().metrics();
        let cur_tasks = metrics.num_alive_tasks();

        _ = dispatcher.receive_bundle(data.to_vec().into()).await;

        // This is horrible, but ensures we actually reach the async parts...
        while metrics.num_alive_tasks() > cur_tasks {
            tokio::task::yield_now().await;
        }
    })
}

fuzz_target!(|data: &[u8]| {
    test_ingress(data);
});

/*
#[test]
fn test() {
    test_ingress(include_bytes!(
        "../artifacts/ingress/crash-da39a3ee5e6b4b0d3255bfef95601890afd80709"
    ));
}
*/

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -o ./fuzz/coverage/ingress/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/ingress/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/ingress -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/ingress/lcov.info
