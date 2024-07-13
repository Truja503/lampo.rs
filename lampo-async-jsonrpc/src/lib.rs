//! Full feature async JSON RPC 2.0 Server/client with a
//! minimal dependencies footprint.
use std::future::Future;
use std::sync::Arc;

use jsonrpsee::server::{RpcModule, RpcServiceBuilder, Server};

pub use jsonrpsee::types::{ErrorObject, ResponsePayload};
pub use jsonrpsee::IntoResponse;

/// JSONRPC v2
pub struct JSONRPCv2<T: Sync + Send + 'static> {
    inner: RpcModule<Arc<T>>,
}

impl<T: Sync + Send + 'static> JSONRPCv2<T> {
    pub fn new(ctx: Arc<T>, path: &str) -> anyhow::Result<Self> {
        Ok(Self {
            inner: RpcModule::new(ctx),
        })
    }

    pub fn add_rpc<R, Fun, Fut>(&mut self, name: &'static str, callback: Fun) -> anyhow::Result<()>
    where
        R: IntoResponse + 'static,
        Fut: Future<Output = R> + Send,
        Fun: (Fn(Arc<T>, serde_json::Value) -> Fut) + Clone + Send + Sync + 'static,
    {
        // FIXME: fix the type definition under here to avoid Arc<Arc< T>>
        self.inner.register_async_method(name, move |params, ctx| {
            let request: serde_json::Value = params.parse().unwrap();
            callback(ctx.as_ref().clone(), request)
        })?;
        Ok(())
    }

    pub async fn listen(self) -> std::io::Result<()> {
        let rpc_middleware = RpcServiceBuilder::new().rpc_logger(1024);
        let server = Server::builder()
            .set_rpc_middleware(rpc_middleware)
            .build("127.0.0.1:9999")
            .await?;
        let addr = server.local_addr()?;
        log::info!("Starting JSON RPC server on {addr}");
        let handle = server.start(self.inner);
        // FIXME: stop the server in a proprer way
        tokio::spawn(handle.stopped());
        Ok(())
    }
}
