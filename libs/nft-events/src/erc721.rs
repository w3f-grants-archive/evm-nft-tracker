//! This module is the entry point for tracking ERC721.
use crate::{erc721_db, erc721_evm, erc721_evm::Erc721Event, Error, EvmClient, Result};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use web3::types::{H160, U256};

use rusqlite::Connection;

/// When the ERC721 event is fetched, the event will be exposed to the caller through this trait.
/// The caller needs to implement this trait and write the code on how to use the event.
/// The metadata is also passed along with it.
#[async_trait]
pub trait Erc721EventCallback: Send {
    /// The callback function
    async fn on_erc721_event(
        &mut self,
        event: Erc721Event,
        name: String,
        symbol: String,
        total_supply: Option<u128>,
        token_uri: String,
    ) -> Result<()>;
}

/// Entry function for tracking ERC721.
/// If you only need to track ERC721, you can use this function directly.
pub async fn track_erc721_events(
    evm_client: &EvmClient,
    db_conn: &Connection,
    start_from: u64,
    step: u64,
    end_block: Option<u64>,
    callback: &mut dyn Erc721EventCallback,
) {
    let mut step = step;
    let mut from = start_from;
    loop {
        match evm_client.get_latest_block_number().await {
            Ok(latest_block_number) => {
                let to = std::cmp::min(from + step - 1, latest_block_number - 6);
                if let Some(end_block) = end_block {
                    if to > end_block {
                        break;
                    }
                }

                if to >= from {
                    debug!(
                        "Scan for {} ERC721 events in block range of {} - {}({})",
                        evm_client.chain_name,
                        from,
                        to,
                        to - from + 1
                    );
                    let start = Instant::now();
                    match erc721_evm::get_erc721_events(&evm_client, from, to).await {
                        Ok(events) => {
                            info!(
                                "{} {} ERC721 events were scanned in block range of {} - {}({})",
                                events.len(),
                                evm_client.chain_name,
                                from,
                                to,
                                to - from + 1
                            );
                            for event in events {
                                // PROCESS AN EVENT
                                if let Err(err) = process_event(evm_client, db_conn, event.clone(), callback).await {
                                    error!("Encountered an error when process ERC721 event {:?} from {}: {:?}.", event, evm_client.chain_name, err);
                                }
                            }

                            from = to + 1;
                            let duration = start.elapsed();
                            debug!("Time elapsed is: {:?}", duration);
                            sleep(Duration::from_secs(5)).await;
                        }
                        Err(err) => match err {
                            Error::Web3Error(web3::Error::Rpc(e)) => {
                                if e.message.contains("more than") {
                                    error!("{}", e.message);
                                    step = std::cmp::max(step / 2, 1);
                                } else {
                                    error!("Encountered an error when get ERC721 events from {}: {:?}, wait for 30 seconds.", evm_client.chain_name, e);
                                    sleep(Duration::from_secs(30)).await;
                                }
                            }
                            _ => {
                                error!("Encountered an error when get ERC721 events from {}: {:?}, wait for 30 seconds.", evm_client.chain_name, err);
                                sleep(Duration::from_secs(30)).await;
                            }
                        },
                    }
                } else {
                    debug!(
                        "Track {} ERC721 events too fast, wait for 30 seconds.",
                        evm_client.chain_name
                    );
                    sleep(Duration::from_secs(30)).await;
                }
            }
            Err(err) => {
                error!("Encountered an error when get latest_block_number from {}: {:?}, wait for 30 seconds.", evm_client.chain_name, err);
                sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

async fn process_event(evm_client: &EvmClient, db_conn: &Connection, event: Erc721Event, callback: &mut dyn Erc721EventCallback) -> Result<()> {
    let metadata = get_metadata(evm_client, db_conn, &event).await?;
    if let Some((name, symbol, token_uri)) = metadata {
        // get total supply
        // let total_supply = evm_client.get_erc721_total_supply(&event.address, event.block_number).await?;
        let total_supply = Some(0);

        // callback
        callback.on_erc721_event(
            event,
            name,
            symbol,
            total_supply,
            token_uri,
        )
        .await?;
    }

    Ok(())
}

async fn get_metadata(
    evm_client: &EvmClient,
    db_conn: &Connection,
    event: &Erc721Event,
) -> Result<Option<(String, String, String)>> {
    save_metadata_to_db_if_not_exists(evm_client, db_conn, &event.address, &event.token_id).await?;
    let collection =
        erc721_db::get_collection_from_db(db_conn, &format!("{:?}", event.address))?.unwrap();
    let token =
        erc721_db::get_token_from_db(db_conn, collection.0, &event.token_id.to_string())?.unwrap();

    if collection.2.is_some() && collection.3.is_some() && token.3.is_some() {
        Ok(Some((collection.2.unwrap(), collection.3.unwrap(), token.3.unwrap())))
    } else {
        Ok(None)
    }
}

async fn save_metadata_to_db_if_not_exists(
    evm_client: &EvmClient,
    db_conn: &Connection,
    address: &H160,
    token_id: &U256,
) -> Result<()> {
    let address_string = format!("{:?}", address);
    let collection_id =
        if let Some(collection) = erc721_db::get_collection_from_db(db_conn, &address_string)? {
            collection.0
        } else {
            if let Some((name, symbol)) = evm_client.get_erc721_name_symbol(address).await? {
                erc721_db::add_collection_to_db(
                    db_conn,
                    address_string.clone(),
                    Some(name),
                    Some(symbol),
                )?
            } else {
                erc721_db::add_collection_to_db(db_conn, address_string.clone(), None, None)?
            }
        };

    let token = erc721_db::get_token_from_db(db_conn, collection_id, &token_id.to_string())?;
    if token.is_none() {
        let token_uri = evm_client.get_erc721_token_uri(address, token_id).await?;
        erc721_db::add_token_to_db(db_conn, token_id.to_string(), collection_id, token_uri)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use web3::{transports::http::Http, Web3};

    struct EthereumErc721EventCallback {
        events: Vec<Erc721Event>,
    }

    #[async_trait]
    impl Erc721EventCallback for EthereumErc721EventCallback {
        async fn on_erc721_event(
            &mut self,
            event: Erc721Event,
            _name: String,
            _symbol: String,
            _total_supply: Option<u128>,
            _token_uri: String,
        ) -> Result<()> {
            self.events.push(event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_track_erc721_events() {
        //
        let web3 = Web3::new(Http::new("https://main-light.eth.linkpool.io").unwrap());
        let client = EvmClient::new("Ethereum".to_owned(), web3);

        //
        let conn = Connection::open("./test7.db").unwrap();
        erc721_db::create_tables_if_not_exist(&conn).unwrap();

        //
        let mut callback = EthereumErc721EventCallback { events: vec![] };
        track_erc721_events(&client, &conn, 13015344, 1, Some(13015346), &mut callback).await;
        assert_eq!(15, callback.events.len());

        std::fs::remove_file("./test7.db").unwrap();
    }
}
