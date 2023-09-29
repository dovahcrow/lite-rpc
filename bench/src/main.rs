use bench::{
    cli::Args,
    helpers::BenchHelper,
    metrics::{AvgMetric, Metric, TxMetricData},
};
use clap::Parser;
use dashmap::DashMap;
use futures::future::join_all;
use log::{error, info, warn};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, signature::Keypair, signer::Signer,
    slot_history::Slot,
};
use std::sync::{atomic::{AtomicU64, Ordering}, Arc};
use tokio::{
    sync::{mpsc::UnboundedSender, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::OnceCell;

#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() {
    tracing_subscriber::fmt::init();

    let Args {
        tx_count,
        runs,
        run_interval_ms,
        metrics_file_name,
        lite_rpc_addr,
        transaction_save_file,
    } = Args::parse();

    let mut run_interval_ms = tokio::time::interval(Duration::from_millis(run_interval_ms));

    info!("Connecting to {lite_rpc_addr}");

    let mut avg_metric = AvgMetric::default();

    let mut tasks = vec![];

    let funded_payer = BenchHelper::get_payer().await.unwrap();
    println!("payer : {}", funded_payer.pubkey());

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        lite_rpc_addr.clone(),
        CommitmentConfig::confirmed(),
    ));
    let bh = rpc_client.get_latest_blockhash().await.unwrap();
    let slot = rpc_client.get_slot().await.unwrap();
    let block_hash: Arc<RwLock<Hash>> = Arc::new(RwLock::new(bh));
    let current_slot = Arc::new(AtomicU64::new(slot));
    {
        // block hash updater task
        let block_hash = block_hash.clone();
        let rpc_client = rpc_client.clone();
        let current_slot = current_slot.clone();
        tokio::spawn(async move {
            loop {
                let bh = rpc_client.get_latest_blockhash().await;
                match bh {
                    Ok(bh) => {
                        let mut lock = block_hash.write().await;
                        *lock = bh;
                    }
                    Err(e) => println!("blockhash update error {}", e),
                }

                let slot = rpc_client.get_slot().await;
                match slot {
                    Ok(slot) => {
                        current_slot.store(slot, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => println!("slot {}", e),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };

    // transaction logger
    let (tx_log_sx, mut tx_log_rx) = tokio::sync::mpsc::unbounded_channel::<TxMetricData>();
    let log_transactions = !transaction_save_file.is_empty();
    if log_transactions {
        tokio::spawn(async move {
            let mut tx_writer = csv::Writer::from_path(transaction_save_file).unwrap();
            while let Some(x) = tx_log_rx.recv().await {
                tx_writer.serialize(x).unwrap();
            }
        });
    }

    for seed in 0..runs {
        let funded_payer = Keypair::from_bytes(funded_payer.to_bytes().as_slice()).unwrap();
        tasks.push(tokio::spawn(bench(
            rpc_client.clone(),
            tx_count,
            funded_payer,
            seed as u64,
            block_hash.clone(),
            current_slot.clone(),
            tx_log_sx.clone(),
            log_transactions,
        )));
        // wait for an interval
        run_interval_ms.tick().await;
    }

    let join_res = join_all(tasks).await;

    let mut run_num = 1;

    let mut csv_writer = csv::Writer::from_path(metrics_file_name).unwrap();
    for res in join_res {
        match res {
            Ok(metric) => {
                info!("Run {run_num}: Sent and Confirmed {tx_count} tx(s) in {metric:?} with",);
                // update avg metric
                avg_metric += &metric;
                csv_writer.serialize(metric).unwrap();
            }
            Err(_) => {
                error!("join error for run {}", run_num);
            }
        }
        run_num += 1;
    }

    let avg_metric = Metric::from(avg_metric);

    info!("Avg Metric {:?}", avg_metric);
    csv_writer.serialize(avg_metric).unwrap();

    csv_writer.flush().unwrap();
}

#[derive(Clone, Debug, Copy)]
struct TxSendData {
    sent_duration: Duration,
    sent_instant: Instant,
    sent_slot: Slot,
}

#[allow(clippy::too_many_arguments)]
async fn bench(
    rpc_client: Arc<RpcClient>,
    tx_count: usize,
    funded_payer: Keypair,
    seed: u64,
    block_hash: Arc<RwLock<Hash>>,
    current_slot: Arc<AtomicU64>,
    tx_metric_sx: UnboundedSender<TxMetricData>,
    log_txs: bool,
) -> Metric {
    let map_of_txs = Arc::new(DashMap::new());
    let total_gross_send_time = Arc::new(OnceCell::new());
    // transaction sender task
    {
        let map_of_txs = map_of_txs.clone();
        let rpc_client = rpc_client.clone();
        let current_slot = current_slot.clone();
        let total_gross_send_time = total_gross_send_time.clone();
        tokio::spawn(async move {
            let map_of_txs = map_of_txs.clone();
            let rand_strings = BenchHelper::generate_random_strings(tx_count, Some(seed));

            let bench_start_time = Instant::now();

            for rand_string in rand_strings {
                let blockhash = { *block_hash.read().await };
                let tx = BenchHelper::create_memo_tx(&rand_string, &funded_payer, blockhash);
                let start_time = Instant::now();
                match rpc_client.send_transaction(&tx).await {
                    Ok(signature) => {
                        map_of_txs.insert(
                            signature,
                            TxSendData {
                                sent_duration: start_time.elapsed(),
                                sent_instant: Instant::now(),
                                sent_slot: current_slot.load(std::sync::atomic::Ordering::Relaxed),
                            },
                        );
                    }
                    Err(e) => {
                        warn!("tx send failed with error {}", e);
                    }
                }
            }
            total_gross_send_time.set(bench_start_time.elapsed()).unwrap();
        });
    } // -- main act: send the transactions


    let mut metric = Metric::default();
    let confirmation_time = Instant::now();
    let mut confirmed_count = 0;
    while confirmation_time.elapsed() < Duration::from_secs(60)
        && !(map_of_txs.is_empty() && confirmed_count == tx_count)
    {
        let signatures = map_of_txs.iter().map(|x| *x.key()).collect::<Vec<_>>();
        if signatures.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        }

        if let Ok(res) = rpc_client.get_signature_statuses(&signatures).await {
            for (i, signature) in signatures.iter().enumerate() {
                let tx_status = &res.value[i];
                if tx_status.is_some() {
                    let tx_data = map_of_txs.get(signature).unwrap();
                    let time_to_confirm = tx_data.sent_instant.elapsed();
                    metric.add_successful_transaction(tx_data.sent_duration, time_to_confirm);

                    if log_txs {
                        let _ = tx_metric_sx.send(TxMetricData {
                            signature: signature.to_string(),
                            sent_slot: tx_data.sent_slot,
                            confirmed_slot: current_slot.load(Ordering::Relaxed),
                            time_to_send_in_millis: tx_data.sent_duration.as_millis() as u64,
                            time_to_confirm_in_millis: time_to_confirm.as_millis() as u64,
                        });
                    }
                    drop(tx_data);
                    map_of_txs.remove(signature);
                    confirmed_count += 1;
                }
            }
        }
    }

    for tx in map_of_txs.iter() {
        metric.add_unsuccessful_transaction(tx.sent_duration);
    }

    metric.set_total_gross_send_time(total_gross_send_time.get().unwrap().as_millis() as f64);

    metric.finalize();
    metric
}
