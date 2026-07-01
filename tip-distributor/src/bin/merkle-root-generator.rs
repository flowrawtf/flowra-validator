//! This binary reads a [StakeMetaCollection] JSON file, generates the merkle trees for each
//! validator's tip-distribution account, and writes the resulting
//! [GeneratedMerkleTreeCollection] to a JSON file.

use {
    clap::Parser,
    log::info,
    solana_client::rpc_client::RpcClient as SyncRpcClient,
    solana_tip_distributor::{
        read_json_from_file, GeneratedMerkleTreeCollection, StakeMetaCollection,
    },
    std::{
        fs::File,
        io::{BufWriter, Write},
        path::PathBuf,
    },
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the JSON file containing the [StakeMetaCollection] object.
    #[arg(long, env)]
    stake_meta_coll_path: PathBuf,

    /// RPC to send transactions through.
    /// Used to validate the expected vs actual claim amounts; optional.
    #[arg(long, env)]
    rpc_url: Option<String>,

    /// Path to the JSON file to write the [GeneratedMerkleTreeCollection] object to.
    #[arg(long, env)]
    out_path: PathBuf,
}

fn main() {
    env_logger::init();

    let args: Args = Args::parse();
    info!("Starting merkle-root-generator...");

    let stake_meta_coll: StakeMetaCollection = read_json_from_file(&args.stake_meta_coll_path)
        .expect("read StakeMetaCollection from stake_meta_coll_path");

    let maybe_rpc_client = args.rpc_url.map(SyncRpcClient::new);

    let merkle_tree_coll =
        GeneratedMerkleTreeCollection::new_from_stake_meta_collection(stake_meta_coll, maybe_rpc_client)
            .expect("generate merkle tree collection");

    let file = File::create(&args.out_path).expect("create out_path file");
    let mut writer = BufWriter::new(file);
    let json = serde_json::to_string_pretty(&merkle_tree_coll).expect("serialize merkle trees");
    writer.write_all(json.as_bytes()).expect("write out_path");
    writer.flush().expect("flush out_path");

    info!("Wrote merkle tree collection to {:?}", args.out_path);
}
