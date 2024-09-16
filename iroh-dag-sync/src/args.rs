use std::{
    net::{SocketAddrV4, SocketAddrV6},
    path::PathBuf,
};

use clap::Parser;
use iroh_net::NodeId;

use crate::protocol::Cid;

#[derive(Debug, Parser)]
pub struct Args {
    #[clap(subcommand)]
    pub cmd: SubCommand,
}

#[derive(Debug, Parser)]
pub enum SubCommand {
    Import(ImportArgs),
    Export(ExportArgs),
    Node(NodeArgs),
    Sync(SyncArgs),
}

#[derive(Debug, Parser)]
pub struct ImportArgs {
    #[clap(help = "The path to the CAR file to import")]
    pub path: PathBuf,
}

#[derive(Debug, Parser)]
pub struct ExportArgs {
    #[clap(help = "The root cid to traverse")]
    pub cid: Option<Cid>,

    #[clap(long, help = "Traversal method to use, full if omitted")]
    pub traversal: Option<String>,

    #[clap(long, help = "The path to the CAR file to export to")]
    pub target: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct NodeArgs {
    #[clap(flatten)]
    pub net: NetArgs,
}

#[derive(Debug, Parser)]
pub struct NetArgs {
    /// The IPv4 addr to listen on.
    #[clap(long)]
    pub iroh_ipv4_addr: Option<SocketAddrV4>,
    /// The IPv6 addr to listen on.
    #[clap(long)]
    pub iroh_ipv6_addr: Option<SocketAddrV6>,
}

#[derive(Debug, Parser)]
pub struct SyncArgs {
    #[clap(flatten)]
    pub net: NetArgs,
    #[clap(help = "The root cid to sync")]
    pub root: Option<Cid>,
    #[clap(long, help = "Traversal method to use, full if omitted")]
    pub traversal: Option<String>,
    #[clap(long, help = "Which data to send inline")]
    pub inline: Option<String>,
    #[clap(long, help = "The node to sync from")]
    pub from: NodeId,
}
