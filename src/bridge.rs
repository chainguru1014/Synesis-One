use crate::{
    configs::SendTransactionConfig,
    encoding::BinaryEncoding,
    rpc::{
        ConfirmTransactionParams, JsonRpcError, JsonRpcReq, JsonRpcRes, RpcMethod,
        SendTransactionParams,
    },
    workers::{BlockListener, TxSender},
    DEFAULT_TX_MAX_RETRIES,
};

use std::{net::ToSocketAddrs, str::FromStr, sync::Arc};

use actix_web::{web, App, HttpServer, Responder};
use reqwest::Url;

use solana_client::{
    nonblocking::{rpc_client::RpcClient, tpu_client::TpuClient},
    rpc_response::RpcVersionInfo,
};
use solana_sdk::{signature::Signature, transaction::VersionedTransaction};
use tokio::task::JoinHandle;

/// A bridge between clients and tpu
pub struct LightBridge {
    pub tpu_client: Arc<TpuClient>,
    pub rpc_url: Url,
    pub tx_sender: TxSender,
    pub block_listner: BlockListener,
}

impl LightBridge {
    pub async fn new(rpc_url: reqwest::Url, ws_addr: &str) -> anyhow::Result<Self> {
        let rpc_client = Arc::new(RpcClient::new(rpc_url.to_string()));

        let tpu_client =
            Arc::new(TpuClient::new(rpc_client.clone(), ws_addr, Default::default()).await?);

        let block_listner = BlockListener::new(rpc_client.clone(), ws_addr).await?;

        Ok(Self {
            tx_sender: TxSender::new(tpu_client.clone(), block_listner.clone()),
            block_listner,
            rpc_url,
            tpu_client,
        })
    }

    pub async fn send_transaction(
        &self,
        SendTransactionParams(
            tx,
            SendTransactionConfig {
                skip_preflight: _,       //TODO:
                preflight_commitment: _, //TODO:
                encoding,
                max_retries,
                min_context_slot: _, //TODO:
            },
        ): SendTransactionParams,
    ) -> Result<String, JsonRpcError> {
        let raw_tx = encoding.decode(tx)?;

        let sig = bincode::deserialize::<VersionedTransaction>(&raw_tx)?.signatures[0];

        self.tpu_client.send_wire_transaction(raw_tx.clone()).await;

        self.tx_sender
            .enqnueue_tx(sig, raw_tx, max_retries.unwrap_or(DEFAULT_TX_MAX_RETRIES))
            .await;

        Ok(BinaryEncoding::Base58.encode(sig))
    }

    pub async fn confirm_transaction(
        &self,
        ConfirmTransactionParams(sig, _): ConfirmTransactionParams,
    ) -> Result<bool, JsonRpcError> {
        let sig = Signature::from_str(&sig)?;

        Ok(self.block_listner.confirm_tx(sig).await.is_some())
    }

    pub fn get_version(&self) -> RpcVersionInfo {
        let version = solana_version::Version::default();
        RpcVersionInfo {
            solana_core: version.to_string(),
            feature_set: Some(version.feature_set),
        }
    }

    /// Serialize params and execute the specified method
    pub async fn execute_rpc_request(
        &self,
        JsonRpcReq { method, params }: JsonRpcReq,
    ) -> Result<serde_json::Value, JsonRpcError> {
        match method {
            RpcMethod::SendTransaction => Ok(self
                .send_transaction(serde_json::from_value(params)?)
                .await?
                .into()),
            RpcMethod::ConfirmTransaction => Ok(self
                .confirm_transaction(serde_json::from_value(params)?)
                .await?
                .into()),
            RpcMethod::GetVersion => Ok(serde_json::to_value(self.get_version()).unwrap()),
            RpcMethod::Other => unreachable!("Other Rpc Methods should be handled externally"),
        }
    }

    /// List for `JsonRpc` requests
    pub fn start_services(
        self,
        addr: impl ToSocketAddrs + Send + 'static,
    ) -> Vec<JoinHandle<anyhow::Result<()>>> {
        let this = Arc::new(self);
        let tx_sender = this.tx_sender.clone().execute();
        let block_listenser = this.block_listner.clone().listen();

        let json_cfg = web::JsonConfig::default().error_handler(|err, req| {
            let err = JsonRpcRes::Err(serde_json::Value::String(format!("{err}")))
                .respond_to(req)
                .into_body();
            actix_web::error::ErrorBadRequest(err)
        });

        let server = tokio::spawn(async move {
            let server = HttpServer::new(move || {
                App::new()
                    .app_data(web::Data::new(this.clone()))
                    .app_data(json_cfg.clone())
                    .route("/", web::post().to(Self::rpc_route))
            })
            .bind(addr)?
            .run();

            server.await?;

            Ok(())
        });

        vec![server, block_listenser, tx_sender]
    }

    async fn rpc_route(body: bytes::Bytes, state: web::Data<Arc<LightBridge>>) -> JsonRpcRes {
        let json_rpc_req = match serde_json::from_slice::<JsonRpcReq>(&body) {
            Ok(json_rpc_req) => json_rpc_req,
            Err(err) => return JsonRpcError::SerdeError(err).into(),
        };

        if let RpcMethod::Other = json_rpc_req.method {
            let res = reqwest::Client::new()
                .post(state.rpc_url.clone())
                .body(body)
                .header("Content-Type", "application/json")
                .send()
                .await
                .unwrap();

            JsonRpcRes::Raw {
                status: res.status().as_u16(),
                body: res.text().await.unwrap(),
            }
        } else {
            state
                .execute_rpc_request(json_rpc_req)
                .await
                .try_into()
                .unwrap()
        }
    }
}
