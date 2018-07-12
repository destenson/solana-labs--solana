extern crate atty;
extern crate bincode;
extern crate bs58;
extern crate clap;
extern crate env_logger;
extern crate serde_json;
extern crate solana;

use bincode::serialize;
use clap::{App, Arg, SubCommand};
use solana::crdt::NodeInfo;
use solana::drone::DroneRequest;
use solana::fullnode::Config;
use solana::signature::{read_keypair, KeyPair, KeyPairUtil, PublicKey, Signature};
use solana::thin_client::ThinClient;
use std::error;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::thread::sleep;
use std::time::Duration;

enum WalletCommand {
    Address,
    Balance,
    AirDrop(i64),
    Pay(i64, PublicKey),
    Confirm(Signature),
}

#[derive(Debug, Clone)]
enum WalletError {
    CommandNotRecognized(String),
    BadParameter(String),
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid")
    }
}

impl error::Error for WalletError {
    fn description(&self) -> &str {
        "invalid"
    }

    fn cause(&self) -> Option<&error::Error> {
        // Generic error, underlying cause isn't tracked.
        None
    }
}

struct WalletConfig {
    leader: NodeInfo,
    id: KeyPair,
    drone_addr: SocketAddr,
    command: WalletCommand,
}

impl Default for WalletConfig {
    fn default() -> WalletConfig {
        let default_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8000);
        WalletConfig {
            leader: NodeInfo::new_leader(&default_addr),
            id: KeyPair::new(),
            drone_addr: default_addr,
            command: WalletCommand::Balance,
        }
    }
}

fn parse_args() -> Result<WalletConfig, Box<error::Error>> {
    let matches = App::new("solana-wallet")
        .arg(
            Arg::with_name("leader")
                .short("l")
                .long("leader")
                .value_name("PATH")
                .takes_value(true)
                .help("/path/to/leader.json"),
        )
        .arg(
            Arg::with_name("keypair")
                .short("k")
                .long("keypair")
                .value_name("PATH")
                .takes_value(true)
                .default_value("~/.config/solana/id.json")
                .help("/path/to/id.json"),
        )
        .subcommand(
            SubCommand::with_name("airdrop")
                .about("Request a batch of tokens")
                .arg(
                    Arg::with_name("tokens")
                        // .index(1)
                        .long("tokens")
                        .value_name("NUMBER")
                        .takes_value(true)
                        .required(true)
                        .help("The number of tokens to request"),
                ),
        )
        .subcommand(
            SubCommand::with_name("pay")
                .about("Send a payment")
                .arg(
                    Arg::with_name("tokens")
                        // .index(2)
                        .long("tokens")
                        .value_name("NUMBER")
                        .takes_value(true)
                        .required(true)
                        .help("the number of tokens to send"),
                )
                .arg(
                    Arg::with_name("to")
                        // .index(1)
                        .long("to")
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .help("The pubkey of recipient"),
                ),
        )
        .subcommand(
            SubCommand::with_name("confirm")
                .about("Confirm your payment by signature")
                .arg(
                    Arg::with_name("signature")
                        .index(1)
                        .value_name("SIGNATURE")
                        .required(true)
                        .help("The transaction signature to confirm"),
                ),
        )
        .subcommand(SubCommand::with_name("balance").about("Get your balance"))
        .subcommand(SubCommand::with_name("address").about("Get your public key"))
        .get_matches();

    let leader: NodeInfo;
    if let Some(l) = matches.value_of("leader") {
        leader = read_leader(l).node_info;
    } else {
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8000);
        leader = NodeInfo::new_leader(&server_addr);
    };

    let id = read_keypair(matches.value_of("keypair").unwrap()).expect("client keypair");

    let mut drone_addr = leader.contact_info.tpu;
    drone_addr.set_port(9900);

    let command = match matches.subcommand() {
        ("airdrop", Some(airdrop_matches)) => {
            let tokens = airdrop_matches.value_of("tokens").unwrap().parse()?;
            Ok(WalletCommand::AirDrop(tokens))
        }
        ("pay", Some(pay_matches)) => {
            let to = if pay_matches.is_present("to") {
                let pubkey_vec = bs58::decode(pay_matches.value_of("to").unwrap())
                    .into_vec()
                    .expect("base58-encoded public key");

                if pubkey_vec.len() != std::mem::size_of::<PublicKey>() {
                    display_actions();
                    Err(WalletError::BadParameter("Invalid public key".to_string()))?;
                }
                PublicKey::clone_from_slice(&pubkey_vec)
            } else {
                id.pubkey()
            };

            let tokens = pay_matches.value_of("tokens").unwrap().parse()?;

            Ok(WalletCommand::Pay(tokens, to))
        }
        ("confirm", Some(confirm_matches)) => {
            let sig_vec = bs58::decode(confirm_matches.value_of("signature").unwrap())
                .into_vec()
                .expect("base58-encoded signature");

            if sig_vec.len() == std::mem::size_of::<Signature>() {
                let sig = Signature::clone_from_slice(&sig_vec);
                Ok(WalletCommand::Confirm(sig))
            } else {
                display_actions();
                Err(WalletError::BadParameter("Invalid signature".to_string()))
            }
        }
        ("balance", Some(_balance_matches)) => Ok(WalletCommand::Balance),
        ("address", Some(_address_matches)) => Ok(WalletCommand::Address),
        ("", None) => {
            display_actions();
            Err(WalletError::CommandNotRecognized(
                "no subcommand given".to_string(),
            ))
        }
        _ => unreachable!(),
    }?;

    Ok(WalletConfig {
        leader,
        id,
        drone_addr, // TODO: Add an option for this.
        command,
    })
}

fn process_command(
    config: &WalletConfig,
    client: &mut ThinClient,
) -> Result<(), Box<error::Error>> {
    match config.command {
        // Check client balance
        WalletCommand::Address => {
            println!("{}", bs58::encode(config.id.pubkey()).into_string());
        }
        WalletCommand::Balance => {
            println!("Balance requested...");
            let balance = client.poll_get_balance(&config.id.pubkey());
            match balance {
                Ok(balance) => {
                    println!("Your balance is: {:?}", balance);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Other => {
                    println!("No account found! Request an airdrop to get started.");
                }
                Err(error) => {
                    println!("An error occurred: {:?}", error);
                }
            }
        }
        // Request an airdrop from Solana Drone;
        // Request amount is set in request_airdrop function
        WalletCommand::AirDrop(tokens) => {
            println!("Airdrop requested...");
            println!("Airdropping {:?} tokens", tokens);
            request_airdrop(&config.drone_addr, &config.id, tokens as u64)?;
            // TODO: return airdrop Result from Drone
            sleep(Duration::from_millis(100));
            println!(
                "Your balance is: {:?}",
                client.poll_get_balance(&config.id.pubkey()).unwrap()
            );
        }
        // If client has positive balance, spend tokens in {balance} number of transactions
        WalletCommand::Pay(tokens, to) => {
            let last_id = client.get_last_id();
            let sig = client.transfer(tokens, &config.id, to, &last_id)?;
            println!("{}", bs58::encode(sig).into_string());
        }
        // Confirm the last client transaction by signature
        WalletCommand::Confirm(sig) => {
            if client.check_signature(&sig) {
                println!("Confirmed");
            } else {
                println!("Not found");
            }
        }
    }
    Ok(())
}

fn display_actions() {
    println!();
    println!("Commands:");
    println!("  address   Get your public key");
    println!("  balance   Get your account balance");
    println!("  airdrop   Request a batch of tokens");
    println!("  pay       Send tokens to a public key");
    println!("  confirm   Confirm your last payment by signature");
    println!();
}

fn read_leader(path: &str) -> Config {
    let file = File::open(path.to_string()).unwrap_or_else(|_| panic!("file not found: {}", path));
    serde_json::from_reader(file).unwrap_or_else(|_| panic!("failed to parse {}", path))
}

fn mk_client(r: &NodeInfo) -> io::Result<ThinClient> {
    let requests_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    let transactions_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    requests_socket
        .set_read_timeout(Some(Duration::new(1, 0)))
        .unwrap();

    Ok(ThinClient::new(
        r.contact_info.rpu,
        requests_socket,
        r.contact_info.tpu,
        transactions_socket,
    ))
}

fn request_airdrop(
    drone_addr: &SocketAddr,
    id: &KeyPair,
    tokens: u64,
) -> Result<(), Box<error::Error>> {
    let mut stream = TcpStream::connect(drone_addr)?;
    let req = DroneRequest::GetAirdrop {
        airdrop_request_amount: tokens,
        client_public_key: id.pubkey(),
    };
    let tx = serialize(&req).expect("serialize drone request");
    stream.write_all(&tx).unwrap();
    // TODO: add timeout to this function, in case of unresponsive drone
    Ok(())
}

fn main() -> Result<(), Box<error::Error>> {
    env_logger::init();
    let config = parse_args()?;
    let mut client = mk_client(&config.leader)?;
    process_command(&config, &mut client)
}
