#![doc = include_str!("../README.md")]
#![warn(clippy::nursery)]
#![warn(clippy::pedantic)]

use axum::extract::Request;
use chrono::Utc;
use freedit::{
    app_router::router,
    config::CONFIG,
    controller::{db_utils::clear_invalid, feed::cron_feed, tantivy::Tan},
    error::AppError,
    DB, VERSION,
};
use futures_util::pin_mut;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info,tantivy=warn"))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // only create snapshot in release mode
    #[cfg(not(debug_assertions))]
    create_snapshot(&DB);

    tokio::spawn(async move {
        loop {
            if let Err(e) = clear_invalid(&DB, "captcha").await {
                error!(%e);
            }
            if let Err(e) = clear_invalid(&DB, "sessions").await {
                error!(%e);
            }
            sleep_seconds(300).await;
        }
    });

    tokio::spawn(async move {
        loop {
            sleep_seconds(600).await;
            if let Err(e) = cron_feed(&DB).await {
                error!(%e);
            }
            if let Err(e) = clear_invalid(&DB, "user_stats").await {
                error!(%e);
            }
            sleep_seconds(3600 * 4).await;
        }
    });

    tokio::spawn(async move {
        let mut tan = Tan::init().unwrap();
        if CONFIG.rebuild_index == Some(true) {
            tan.rebuild_index(&DB).unwrap();
        }
        let mut subscriber = DB.open_tree("tan").unwrap().watch_prefix(vec![]);
        while let Some(event) = (&mut subscriber).await {
            let (k, op_type) = match event {
                sled::Event::Insert { key, value: _ } => (key, "add"),
                sled::Event::Remove { key } => (key, "delete"),
            };
            let id = String::from_utf8_lossy(&k);

            if op_type == "add" {
                tan.add_doc(&id, &DB).unwrap();
            }

            tan.commit().unwrap();
        }
    });

    let app = router().await;
    let addr: SocketAddr = CONFIG.addr.parse().unwrap();
    let listener = TcpListener::bind(addr).await.unwrap();

    if let Some(tls_config) = CONFIG.tls_config().await {
        let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));
        info!("listening on https://{}", addr);
        pin_mut!(listener);
        loop {
            let tower_service = app.clone();
            let tls_acceptor = tls_acceptor.clone();
            let (cnx, addr) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let Ok(stream) = tls_acceptor.accept(cnx).await else {
                    error!("error during tls handshake connection from {}", addr);
                    return;
                };
                let stream = TokioIo::new(stream);

                let hyper_service =
                    hyper::service::service_fn(move |request: Request<Incoming>| {
                        tower_service.clone().call(request)
                    });

                let ret = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(stream, hyper_service)
                    .await;

                if let Err(err) = ret {
                    error!("error serving connection from {}: {}", addr, err);
                }
            });
        }
    } else {
        warn!("enable https failed, please check config toml cert and key");
        info!("listening on http://{}", addr);
        axum::serve(listener, app).await.unwrap();
    }

    Ok(())
}

// TODO: TEST with https://github.com/hatoo/oha
#[allow(dead_code)]
fn create_snapshot(db: &sled::Db) {
    let checksum = db.checksum().unwrap();
    info!(%checksum);

    let ts = Utc::now().format("%Y-%m-%d-%H-%M-%S");
    let mut snapshot_path = PathBuf::from("snapshots");
    if !snapshot_path.exists() {
        fs::create_dir_all(&snapshot_path).unwrap();
    }
    snapshot_path.push(format!("{VERSION}-{ts}-{checksum}"));
    let snapshot_cfg = sled::Config::default().path(&snapshot_path);
    let snapshot = snapshot_cfg.open().unwrap();
    snapshot.import(db.export());
    info!("create snapshot: {}", snapshot_path.display());
    drop(snapshot);
}

async fn sleep_seconds(seconds: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
}
