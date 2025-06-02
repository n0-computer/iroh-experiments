//! Command line arguments.
use std::net::{SocketAddrV4, SocketAddrV6};

use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    #[clap(long)]
    pub ipv4_bind_addr: Option<SocketAddrV4>,

    #[clap(long)]
    pub ipv6_bind_addr: Option<SocketAddrV6>,

    #[clap(long)]
    pub quiet: bool,
}
