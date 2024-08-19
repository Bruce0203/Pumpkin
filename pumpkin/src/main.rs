use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};
use std::io::{self};

use client::Client;
use commands::handle_command;
use config::AdvancedConfiguration;

use std::{collections::HashMap, rc::Rc, thread};

use client::interrupted;
use config::BasicConfiguration;
use server::Server;

// Setup some tokens to allow us to identify which event is for which socket.

pub mod client;
pub mod commands;
pub mod config;
pub mod entity;
pub mod proxy;
pub mod rcon;
pub mod server;
pub mod util;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(not(target_os = "wasi"))]
fn main() -> io::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
    #[cfg(feature = "dhat-heap")]
    println!("Using a memory profiler");

    adjust_file_descriptor_limits();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    // ensure rayon is built outside of tokio scope
    rayon::ThreadPoolBuilder::new().build_global().unwrap();
    rt.block_on(async {
        const SERVER: Token = Token(0);
        use std::{cell::RefCell, time::Instant};

        use rcon::RCONServer;

        let time = Instant::now();
        let basic_config = BasicConfiguration::load("configuration.toml");

        let advanced_configuration = AdvancedConfiguration::load("features.toml");

        simple_logger::SimpleLogger::new().init().unwrap();

        // Create a poll instance.
        let mut poll = Poll::new()?;
        // Create storage for events.
        let mut events = Events::with_capacity(128);

        // Setup the TCP server socket.

        let addr = format!(
            "{}:{}",
            basic_config.server_address, basic_config.server_port
        )
        .parse()
        .unwrap();

        let mut listener = TcpListener::bind(addr)?;

        // Register the server with poll we can receive events for it.
        poll.registry()
            .register(&mut listener, SERVER, Interest::READABLE)?;

        // Unique token for each incoming connection.
        let mut unique_token = Token(SERVER.0 + 1);

        let use_console = advanced_configuration.commands.use_console;
        let rcon = advanced_configuration.rcon.clone();

        let mut connections: HashMap<Token, Rc<RefCell<Client>>> = HashMap::new();

        let mut server = Server::new((basic_config, advanced_configuration));
        log::info!("Started Server took {}ms", time.elapsed().as_millis());
        log::info!("You now can connect to the server, Listening on {}", addr);

        if use_console {
            thread::spawn(move || {
                let stdin = std::io::stdin();
                loop {
                    let mut out = String::new();
                    stdin
                        .read_line(&mut out)
                        .expect("Failed to read console line");
                    handle_command(&mut commands::CommandSender::Console, &out);
                }
            });
        }
        if rcon.enabled {
            tokio::spawn(async move {
                RCONServer::new(&rcon).await.unwrap();
            });
        }
        loop {
            if let Err(err) = poll.poll(&mut events, None) {
                if interrupted(&err) {
                    continue;
                }
                return Err(err);
            }

            for event in events.iter() {
                match event.token() {
                    SERVER => loop {
                        // Received an event for the TCP server socket, which
                        // indicates we can accept an connection.
                        let (mut connection, address) = match listener.accept() {
                            Ok((connection, address)) => (connection, address),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // If we get a `WouldBlock` error we know our
                                // listener has no more incoming connections queued,
                                // so we can return to polling and wait for some
                                // more.
                                break;
                            }
                            Err(e) => {
                                // If it was any other kind of error, something went
                                // wrong and we terminate with an error.
                                return Err(e);
                            }
                        };
                        if let Err(e) = connection.set_nodelay(true) {
                            log::warn!("failed to set TCP_NODELAY {e}");
                        }

                        log::info!("Accepted connection from: {}", address);

                        let token = next(&mut unique_token);
                        poll.registry().register(
                            &mut connection,
                            token,
                            Interest::READABLE.add(Interest::WRITABLE),
                        )?;
                        let rc_token = Rc::new(token);
                        let client = Rc::new(RefCell::new(Client::new(
                            Rc::clone(&rc_token),
                            connection,
                            addr,
                        )));
                        server.add_client(rc_token, Rc::clone(&client));
                        connections.insert(token, client);
                    },

                    token => {
                        // Maybe received an event for a TCP connection.
                        let done = if let Some(client) = connections.get_mut(&token) {
                            let mut client = client.borrow_mut();
                            client.poll(&mut server, event).await;
                            client.closed
                        } else {
                            // Sporadic events happen, we can safely ignore them.
                            false
                        };
                        if done {
                            if let Some(client) = connections.remove(&token) {
                                server.remove_client(&token);
                                let mut client = client.borrow_mut();
                                poll.registry().deregister(&mut client.connection)?;
                            }
                        }
                    }
                }
            }
        }
    })
}

fn adjust_file_descriptor_limits() {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        panic!(
            "Failed to get the current file handle limits {}",
            std::io::Error::last_os_error()
        );
    };

    let limit_before = limits.rlim_cur;
    limits.rlim_cur = limits.rlim_max;

    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) } != 0 {
        panic!(
            "Failed to set the file handle limits {}",
            std::io::Error::last_os_error()
        );
    }

    log::debug!(
        "file descriptor adjusted to {} from {}",
        limits.rlim_max,
        limit_before
    );
}

fn next(current: &mut Token) -> Token {
    let next = current.0;
    current.0 += 1;
    Token(next)
}

#[cfg(target_os = "wasi")]
fn main() {
    panic!("can't bind to an address with wasi")
}
