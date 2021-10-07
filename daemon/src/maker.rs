use anyhow::{Context, Result};
use bdk::bitcoin;
use bdk::bitcoin::secp256k1::schnorrsig;
use clap::Clap;
use daemon::auth::{self, MAKER_USERNAME};
use daemon::db::{self, load_all_cfds};
use daemon::model::cfd::{Order, UpdateCfdProposals};
use daemon::model::WalletInfo;
use daemon::seed::Seed;
use daemon::wallet::Wallet;
use daemon::{
    bitmex_price_feed, housekeeping, logger, maker_cfd, maker_inc_connections, monitor, oracle,
    wallet_sync,
};
use rocket::fairing::AdHoc;
use rocket_db_pools::Database;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::watch;
use tracing_subscriber::filter::LevelFilter;
use xtra::prelude::*;
use xtra::spawn::TokioGlobalSpawnExt;

mod routes_maker;

#[derive(Database)]
#[database("maker")]
pub struct Db(sqlx::SqlitePool);

#[derive(Clap)]
struct Opts {
    /// The port to listen on for p2p connections.
    #[clap(long, default_value = "9999")]
    p2p_port: u16,

    /// The port to listen on for the HTTP API.
    #[clap(long, default_value = "8001")]
    http_port: u16,

    /// Where to permanently store data, defaults to the current working directory.
    #[clap(long)]
    data_dir: Option<PathBuf>,

    /// Generate a seed file within the data directory.
    #[clap(long)]
    generate_seed: bool,

    /// If enabled logs will be in json format
    #[clap(short, long)]
    json: bool,

    #[clap(subcommand)]
    network: Network,
}

#[derive(Clap)]
enum Network {
    /// Run on mainnet.
    Mainnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://electrum.blockstream.info:50002")]
        electrum: String,
    },
    /// Run on testnet.
    Testnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://electrum.blockstream.info:60002")]
        electrum: String,
    },
}

impl Network {
    fn electrum(&self) -> &str {
        match self {
            Network::Mainnet { electrum } => electrum,
            Network::Testnet { electrum } => electrum,
        }
    }

    fn bitcoin_network(&self) -> bitcoin::Network {
        match self {
            Network::Mainnet { .. } => bitcoin::Network::Bitcoin,
            Network::Testnet { .. } => bitcoin::Network::Testnet,
        }
    }

    fn data_dir(&self, base: PathBuf) -> PathBuf {
        match self {
            Network::Mainnet { .. } => base.join("mainnet"),
            Network::Testnet { .. } => base.join("testnet"),
        }
    }
}

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    logger::init(LevelFilter::DEBUG, opts.json).context("initialize logger")?;

    let data_dir = opts
        .data_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("unable to get cwd"));

    let data_dir = opts.network.data_dir(data_dir);

    if !data_dir.exists() {
        tokio::fs::create_dir_all(&data_dir).await?;
    }

    let seed = Seed::initialize(&data_dir.join("maker_seed"), opts.generate_seed).await?;

    let ext_priv_key = seed.derive_extended_priv_key(opts.network.bitcoin_network())?;

    let wallet = Wallet::new(
        opts.network.electrum(),
        &data_dir.join("maker_wallet.sqlite"),
        ext_priv_key,
    )
    .await?;
    let wallet_info = wallet.sync().await.unwrap();

    let auth_password = seed.derive_auth_password::<auth::Password>();

    tracing::info!(
        "Authentication details: username='{}' password='{}'",
        MAKER_USERNAME,
        auth_password
    );

    // TODO: Actually fetch it from Olivia
    let oracle = schnorrsig::PublicKey::from_str(
        "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7",
    )?;

    let (order_feed_sender, order_feed_receiver) = watch::channel::<Option<Order>>(None);
    let (wallet_feed_sender, wallet_feed_receiver) = watch::channel::<WalletInfo>(wallet_info);
    let (update_cfd_feed_sender, update_cfd_feed_receiver) =
        watch::channel::<UpdateCfdProposals>(HashMap::new());

    let figment = rocket::Config::figment()
        .merge(("databases.maker.url", data_dir.join("maker.sqlite")))
        .merge(("port", opts.http_port));

    let listener = tokio::net::TcpListener::bind(&format!("0.0.0.0:{}", opts.p2p_port)).await?;
    let local_addr = listener.local_addr().unwrap();

    tracing::info!("Listening on {}", local_addr);

    let (task, quote_updates) = bitmex_price_feed::new().await?;
    tokio::spawn(task);

    rocket::custom(figment)
        .manage(order_feed_receiver)
        .manage(wallet_feed_receiver)
        .manage(update_cfd_feed_receiver)
        .manage(auth_password)
        .manage(quote_updates)
        .attach(Db::init())
        .attach(AdHoc::try_on_ignite(
            "SQL migrations",
            |rocket| async move {
                match Db::fetch(&rocket) {
                    Some(db) => match db::run_migrations(&**db).await {
                        Ok(_) => Ok(rocket),
                        Err(_) => Err(rocket),
                    },
                    None => Err(rocket),
                }
            },
        ))
        .attach(AdHoc::try_on_ignite(
            "Create actors",
            move |rocket| async move {
                let db = match Db::fetch(&rocket) {
                    Some(db) => (**db).clone(),
                    None => return Err(rocket),
                };
                let mut conn = db.acquire().await.unwrap();

                housekeeping::transition_non_continue_cfds_to_setup_failed(&mut conn)
                    .await
                    .unwrap();
                housekeeping::rebroadcast_transactions(&mut conn, &wallet)
                    .await
                    .unwrap();
                let cfds = load_all_cfds(&mut conn).await.unwrap();

                let (cfd_feed_sender, cfd_feed_receiver) = watch::channel(cfds.clone());
                let (maker_inc_connections_address, maker_inc_connections_context) =
                    xtra::Context::new(None);

                let (monitor_actor_address, mut monitor_actor_context) = xtra::Context::new(None);

                let (oracle_actor_address, mut oracle_actor_context) = xtra::Context::new(None);

                let mut conn = db.acquire().await.unwrap();
                let cfds = load_all_cfds(&mut conn).await.unwrap();
                let cfd_maker_actor_inbox = maker_cfd::Actor::new(
                    db,
                    wallet.clone(),
                    oracle,
                    cfd_feed_sender,
                    order_feed_sender,
                    update_cfd_feed_sender,
                    maker_inc_connections_address.clone(),
                    monitor_actor_address.clone(),
                    oracle_actor_address.clone(),
                )
                .create(None)
                .spawn_global();

                tokio::spawn(
                    maker_inc_connections_context.run(maker_inc_connections::Actor::new(
                        cfd_maker_actor_inbox.clone(),
                    )),
                );
                tokio::spawn(
                    monitor_actor_context
                        .notify_interval(Duration::from_secs(20), || monitor::Sync)
                        .unwrap(),
                );
                tokio::spawn(
                    monitor_actor_context.run(
                        monitor::Actor::new(
                            opts.network.electrum(),
                            cfd_maker_actor_inbox.clone(),
                            cfds.clone(),
                        )
                        .await
                        .unwrap(),
                    ),
                );

                tokio::spawn(
                    oracle_actor_context
                        .notify_interval(Duration::from_secs(60), || oracle::Sync)
                        .unwrap(),
                );
                tokio::spawn(oracle_actor_context.run(oracle::Actor::new(
                    cfd_maker_actor_inbox.clone(),
                    monitor_actor_address,
                    cfds,
                )));

                // use `.send` here to ensure we only continue once the update was processed
                oracle_actor_address.send(oracle::Sync).await.unwrap();

                let listener_stream = futures::stream::poll_fn(move |ctx| {
                    let message = match futures::ready!(listener.poll_accept(ctx)) {
                        Ok((stream, address)) => {
                            maker_inc_connections::ListenerMessage::NewConnection {
                                stream,
                                address,
                            }
                        }
                        Err(e) => maker_inc_connections::ListenerMessage::Error { source: e },
                    };

                    Poll::Ready(Some(message))
                });

                tokio::spawn(maker_inc_connections_address.attach_stream(listener_stream));
                tokio::spawn(wallet_sync::new(wallet, wallet_feed_sender));

                Ok(rocket
                    .manage(cfd_maker_actor_inbox)
                    .manage(cfd_feed_receiver))
            },
        ))
        .mount(
            "/api",
            rocket::routes![
                routes_maker::maker_feed,
                routes_maker::post_sell_order,
                routes_maker::post_cfd_action,
                routes_maker::get_health_check
            ],
        )
        .register("/api", rocket::catchers![routes_maker::unauthorized])
        .mount(
            "/",
            rocket::routes![routes_maker::dist, routes_maker::index],
        )
        .register("/", rocket::catchers![routes_maker::unauthorized])
        .launch()
        .await?;

    Ok(())
}
