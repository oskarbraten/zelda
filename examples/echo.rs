use anyhow::Result;
use rcgen::generate_simple_self_signed;
use tokio::time::{sleep, Duration};
use tokio_rustls::{
    rustls::{
        internal::pemfile::{certs, pkcs8_private_keys},
        ClientConfig, NoClientAuth, ServerConfig,
    },
    webpki::DNSNameRef,
};
use zelda::{Client, Config, Event, Server};

fn main() -> Result<()> {
    env_logger::init();

    let address = "127.0.0.1:10000";

    let generated_certificate = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

    let server_config = {
        let mut config = ServerConfig::new(NoClientAuth::new());
        let certificates = certs(&mut generated_certificate.serialize_pem()?.as_bytes()).unwrap();

        let keys =
            pkcs8_private_keys(&mut generated_certificate.serialize_private_key_pem().as_bytes())
                .unwrap();

        config.set_single_cert(certificates, keys[0].clone())?;
        config
    };

    let client_config = {
        let mut config = ClientConfig::new();
        config
            .root_store
            .add_pem_file(&mut generated_certificate.serialize_pem()?.as_bytes())
            .unwrap();
        config
    };

    let client_domain = DNSNameRef::try_from_ascii_str("localhost")
        .unwrap()
        .to_owned();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (server_sender, server_receiver, server_task) =
                Server::listen(address, Config::default(), server_config);
            let (r1, r2) = tokio::join!(
                tokio::spawn(server_task),
                tokio::spawn(async move {
                    loop {
                        match server_receiver.recv().await {
                            Ok(event) => match event {
                                (id, Event::Connected) => {
                                    log::info!("SERVER: Client {}, connected!", id);
                                }
                                (id, Event::Received(data)) => {
                                    log::info!(
                                        "SERVER: received: {} (Connection id: {})",
                                        std::str::from_utf8(&data).unwrap(),
                                        id,
                                    );

                                    let mut data = data;
                                    data.extend(b"- ECHO.");
                                    server_sender.reliable(id, data).unwrap();
                                }
                                (id, Event::Disconnected) => {
                                    log::info!("SERVER: Client {}, disconnected!", id);
                                }
                            },
                            Err(err) => {
                                log::debug!("{}", err);
                                break;
                            }
                        }
                    }
                })
            );

            r1.unwrap().unwrap();
            r2.unwrap();
        });
    });

    std::thread::sleep(Duration::from_millis(500));

    let t2 = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (client_sender, client_receiver, client_task) =
                Client::connect(address, Config::default(), client_domain, client_config);

            client_sender
                .reliable(b"This message was sent before being connected.".to_vec())
                .unwrap();

            tokio::select! {
                _ = sleep(Duration::from_millis(5000)) => {},
                result = client_task => {
                    log::info!("CLIENT: Task completed with result: {:#?}", result);
                },
                _ = tokio::spawn(async move {
                    loop {
                        match client_receiver.recv().await {
                            Ok(event) => match event {
                                Event::Connected => {
                                    log::info!("CLIENT: Connected to server!");

                                    let client_sender = client_sender.clone();
                                    tokio::spawn(async move {
                                        loop {
                                            tokio::time::sleep(Duration::from_millis(500)).await;
                                            if rand::random::<f32>() > 0.5 {
                                                client_sender
                                                    .reliable(b"Hello, world!".to_vec())
                                                    .unwrap();
                                            } else {
                                                client_sender
                                                    .unreliable(b"Hello, world!".to_vec())
                                                    .unwrap();
                                            }
                                        }
                                    });
                                }
                                Event::Received(data) => {
                                    log::info!(
                                        "CLIENT: Received from server: {}",
                                        std::str::from_utf8(&data).unwrap()
                                    );
                                }
                                Event::Disconnected => {
                                    log::info!("CLIENT: Disconnected from server!");
                                }
                            },
                            Err(err) => {
                                log::debug!("{}", err);
                                break;
                            }
                        }
                    }
                }) => {}
            }
        });
    });

    t2.join().unwrap();

    Ok(())
}
