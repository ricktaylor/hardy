#![no_main]

use hardy_tcpcl::*;
use libfuzzer_sys::fuzz_target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
static CONFIG: std::sync::OnceLock<config::Config> = std::sync::OnceLock::new();

fn get_config() -> &'static config::Config {
    CONFIG.get_or_init(|| {
        let mut filename = std::env::current_dir().unwrap();
        filename.push("fuzz/passive.config");

        config::Config::builder()
            .add_source(
                config::File::from(filename)
                    .format(config::FileFormat::Toml)
                    .required(false),
            )
            .build()
            .unwrap()
    })
}

fn get_addr() -> std::net::SocketAddr {
    match get_config().get("tcp_address") {
        Ok(r) => r,
        Err(config::ConfigError::NotFound(_)) => std::net::SocketAddr::V6(
            std::net::SocketAddrV6::new(std::net::Ipv6Addr::LOCALHOST, 4556, 0, 0),
        ),
        Err(e) => panic!("Invalid 'tcp_address' value in configuration {}", e),
    }
}

fn setup() -> tokio::runtime::Runtime {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.spawn(async {
        let config = get_config();
        utils::logger::init(config);

        let (mut task_set, cancel_token) = utils::cancel::new_cancellable_set();

        listener::init(config, bpa::Bpa::new(config), &mut task_set, cancel_token);

        while task_set.join_next().await.is_some() {}
    });

    rt
}

fuzz_target!(|data: &[u8]| {
    RT.get_or_init(setup).block_on(async {
        let mut i = 0;
        let (mut rx, mut tx) = loop {
            match tokio::net::TcpStream::connect(get_addr()).await {
                Ok(stream) => break stream.into_split(),
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::ConnectionRefused {
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        i += 1;
                        if i < 10 {
                            continue;
                        }
                    }
                    panic!("Failed to connect: {}", e);
                }
            }
        };

        let h = tokio::task::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match rx.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
            }
        });

        tx.write_all(data).await.unwrap();
        let _ = tx.shutdown().await;

        h.await.unwrap();
    })
});
