#[macro_use]
extern crate log;
extern crate env_logger;
extern crate tokio;
extern crate tokio_io;
extern crate futures;
extern crate bytes;
extern crate postgres;
extern crate r2d2;
extern crate r2d2_postgres;
extern crate chrono;
#[macro_use]
extern crate clap;
extern crate regex;

use std::sync::{Arc, Mutex};
use std::net::SocketAddr;

use tokio::io::{write_all, shutdown};
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use r2d2_postgres::{TlsMode, PostgresConnectionManager};

mod net;
mod codec;
mod color;
mod protocol;
mod db;

use codec::*;
use db::*;

fn main() {
    env_logger::init();

    let matches = clap_app!(("BCProxy Rust") =>
                            (version: "0.1")
                            (@arg listen: -l --listen +takes_value "address and port to listen on")
                            (@arg server: -s --server +takes_value "BatMUD server")
                            (@arg mapper: --mapper +takes_value "address to listen on for mapper")
                            (@arg db: --db +takes_value "postgresql://[user[:password]@][netloc][:port][/dbname][?param1=value1&...]")
                            (@arg monster: --monster ... "parse and save monster info")
                           ).get_matches();

    info!("Connecting to database");
    let pool = matches.value_of("db").map(|url| {
        let manager = PostgresConnectionManager::new(url, TlsMode::None).unwrap();
        r2d2::Pool::new(manager).unwrap()
    });

    if pool.is_none() {
        warn!("No DB connection created. Room data will NOT be saved!");
    }

    let parse_monster = matches.is_present("monster");

    let listen_addr = matches.value_of("listen").map_or("127.0.0.1:9999".to_string(), &str::to_string);
    let listen_addr = listen_addr.parse::<SocketAddr>().unwrap();

    let bat_addr = matches.value_of("server").map_or("83.145.249.153:2023".to_string(), &str::to_string);
    let bat_addr = bat_addr.parse::<SocketAddr>().unwrap();

    let mapper_addr = matches.value_of("mapper").map_or("127.0.0.1:0".to_string(), &str::to_string);
    let mapper_addr = mapper_addr.parse::<SocketAddr>().unwrap();

    // Listen for incoming connections.
    info!("Listening on: {}", listen_addr);
    let socket = TcpListener::bind(&listen_addr).unwrap();

    let done = socket.incoming()
        .map_err(|e| error!("Error accepting socket; error = {}", e))
        .for_each(move |client| {
            info!("Connecting to: {}", bat_addr);
            let bat = TcpStream::connect(&bat_addr);

            let mapper = TcpListener::bind(&mapper_addr).unwrap();
            info!("Listening for mapper on: {:?}", mapper.local_addr());

            let mapper_clients: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));

            let mapper_clients_inner = mapper_clients.clone();
            let mapper_done = mapper.incoming()
                .map_err(|e| error!("Error accepting mapper; error = {}", e))
                .for_each(move |mapper_client| {
                    mapper_clients_inner.lock().unwrap().push(mapper_client);
                    Ok(())
                });

            tokio::spawn(mapper_done);

            let client_to_bat_db = pool.clone().map(|p| Db::new(p));
            let bat_to_client_db = pool.clone().map(|p| Db::new(p));

            let amounts = bat.and_then(move |bat| {
                let client_reader = net::ProxyTcpStream(Arc::new(Mutex::new(client)));
                let client_writer = client_reader.clone();
                let bat_reader = net::ProxyTcpStream(Arc::new(Mutex::new(bat)));
                let bat_writer = bat_reader.clone();

                let bc_mode = write_all(bat_writer.clone(),  [0x1b, b'b', b'c', b' ', b'1', b'\n']);

                let mut bat_writer_inner = bat_writer.clone();
                let client_to_bat = client_reader.framed(LinesCodec::new())
                    .and_then(move |frame| {
                        match frame {
                            SendFrame::Line(bytes) => bat_writer_inner.write(&bytes[..]),
                            SendFrame::MonsterExp(name, area, exp) => {
                                if client_to_bat_db.is_some() {
                                    match client_to_bat_db.as_ref().unwrap().update_monster_exp(name, area, exp) {
                                        Ok(_) => (),
                                        Err(e) => error!("failed to update monster exp: {}", e),
                                    }
                                }

                                Ok(0)
                            },
                            _ => {
                                error!("failed to parse send frame");
                                Ok(0)
                            }
                        }
                    })
                .fold(0usize, |acc, x| future::ok::<_, tokio::io::Error>(acc + x))
                    .and_then(move |n| {
                        shutdown(bat_writer).map(move |_| n)
                    });

                let mut client_writer_mut = client_writer.clone();
                let bat_to_client = bat_reader.framed(BatCodec::new(parse_monster))
                    .and_then(move |frame| {
                        match frame {
                            BatFrame::Bytes(bytes) => client_writer_mut.write(&bytes[..]),
                            BatFrame::Code(code) => client_writer_mut.write(&code.to_bytes()[..]),
                            BatFrame::BatMapper(mapper) => {
                                if bat_to_client_db.is_some() && mapper.id.is_some() {
                                    match bat_to_client_db.as_ref().unwrap().save_bat_mapper_room(&mapper) {
                                        Ok(_) => (),
                                        Err(e) => error!("failed to save room: {}", e),
                                    }
                                }

                                for mut client in mapper_clients.lock().unwrap().iter() {
                                    match client.write(&mapper.raw[..]) {
                                        Ok(_) => (),
                                        Err(e) => error!("failed to write to mapper: {}", e),
                                    };
                                };

                                client_writer_mut.write(&mapper.output[..])
                            },

                            BatFrame::Nothing => client_writer_mut.write(&[][..]),
                        }
                    })
                    .fold(0usize, |acc, x| future::ok::<_, tokio::io::Error>(acc + x))
                    .and_then(move |n| {
                        shutdown(client_writer).map(move |_| n)
                    });

                bc_mode.and_then(|_| client_to_bat.join(bat_to_client))
            });

            let msg = amounts.map(|(from_client, from_bat)| {
                info!("Client wrote {} bytes and received {} bytes", from_client, from_bat);
            }).map_err(|e| {
                // Don't panic. Maybe the client just disconnected too soon.
                error!("Error: {}", e);
            });

            tokio::spawn(msg);

            Ok(())
        });

    tokio::run(done);
}
