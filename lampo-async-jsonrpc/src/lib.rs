//! Full feature async JSON RPC 2.0 Server/client with a
//! minimal dependencies footprint.
#![feature(type_alias_impl_trait)]
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::os::unix::prelude::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::io::{self, AsyncReadExt};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub mod command;
pub mod errors;
pub mod json_rpc2;

use crate::errors::Error;
use crate::errors::RpcError;
use crate::json_rpc2::{Request, Response};

type AsyncFn<T> = impl Fn(&T, Value) -> AsyncFuture;
type AsyncFuture = impl Future<Output = Result<Value, Error>> + Send + 'static;

/// JSONRPC v2
pub struct JSONRPCv2<T: Send + Sync + 'static> {
    socket_path: String,
    handler: Arc<Handler<T>>,
}

pub struct Handler<T: Send + Sync + 'static> {
    stop: Cell<bool>,
    rpc_method: RefCell<HashMap<String, AsyncFn<T>>>,
    ctx: Arc<T>,
}

unsafe impl<T: Send + Sync> Sync for Handler<T> {}
unsafe impl<T: Send + Sync> Send for Handler<T> {}

impl<T: Send + Sync + 'static> Handler<T> {
    pub fn new(ctx: Arc<T>) -> Self {
        Handler::<T> {
            stop: Cell::new(false),
            rpc_method: RefCell::new(HashMap::new()),
            ctx,
        }
    }

    pub fn add_method(&self, method: &str, callback: AsyncFn<T>) {
        self.rpc_method
            .borrow_mut()
            .insert(method.to_owned(), callback);
    }

    pub async fn run_callback(&self, req: &Request<Value>) -> Option<Result<Value, errors::Error>> {
        let binding = self.rpc_method.take();
        let Some(callback) = binding.get(&req.method) else {
            return Some(Err(errors::RpcError {
                message: format!("method `{}` not found", req.method),
                code: -1,
                data: None,
            }
            .into()));
        };
        let resp = callback(&self.ctx, req.params.clone()).await;
        Some(resp)
    }

    pub fn has_rpc(&self, method: &str) -> bool {
        self.rpc_method.borrow().contains_key(method)
    }

    pub fn stop(&self) {
        self.stop.set(true);
    }
}

impl<T: Send + Sync + 'static> JSONRPCv2<T> {
    pub fn new(ctx: Arc<T>, path: &str) -> Result<Self, Error> {
        Ok(Self {
            handler: Arc::new(Handler::new(ctx)),
            socket_path: path.to_owned(),
        })
    }

    pub fn add_rpc<F, Fut>(&self, name: &str, callback: AsyncFn<T>) -> Result<(), ()> {
        if self.handler.has_rpc(name) {
            return Err(());
        }
        self.handler.add_method(name, callback);
        Ok(())
    }

    async fn handle_request(
        handler: Arc<Handler<T>>,
        payload: Request<Value>,
    ) -> io::Result<Response<Value>> {
        log::debug!(
            "request received `{}`",
            serde_json::to_string(&payload).unwrap()
        );
        if payload.jsonrpc != "2.0" {
            return Ok(Response {
                jsonrpc: "2.0".to_string(),
                result: None,
                error: Some(RpcError {
                    code: -32600,
                    message: format!(
                        "Invalid reuqest: The JSON sent is not a valid Request object."
                    ),
                    // FIXME: remove the clone here
                    data: Some(serde_json::to_value(payload.clone()).unwrap()),
                }),
                id: payload.id.clone(),
            });
        }
        // TODO: return an error
        let resp = handler.run_callback(&payload).await.unwrap();
        let resp = Self::write(payload, resp).unwrap();

        log::debug!(
            "response received `{}`",
            serde_json::to_string(&resp).unwrap()
        );
        Ok(resp)
    }

    fn write(
        request: Request<Value>,
        resp: Result<Value, errors::Error>,
    ) -> io::Result<Response<Value>> {
        let resp = match resp {
            Ok(resp) => Response {
                id: request.id,
                jsonrpc: "2.0".to_string(),
                error: None,
                result: Some(resp),
            },
            Err(val) => Response {
                result: None,
                error: Some(val.into()),
                id: request.id.to_owned(),
                jsonrpc: "2.0".to_string(),
            },
        };
        Ok(resp)
    }

    pub async fn spawn(self) -> JoinHandle<io::Result<()>> {
        tokio::spawn(async { self.listen().await })
    }

    pub async fn listen(self) -> std::io::Result<()> {
        log::info!("starting server on {}", self.socket_path);
        let socket_path = PathBuf::from(&self.socket_path);
        // Remove old socket if it exists
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))?;

        while !self.handler.stop.get() {
            let (mut socket, _) = listener.accept().await.unwrap();
            let handler = self.handler();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                log::trace!("Start reading");
                if let Ok(_) = socket.read_buf(&mut buffer).await {
                    if let Ok(request) = serde_json::from_slice::<Request<Value>>(&buffer) {
                        let response = Self::handle_request(handler, request).await.unwrap();
                        let response_bytes = serde_json::to_vec(&response).unwrap();
                        let _ = socket.write_all(&response_bytes).await;
                    }
                }
            })
            .await;
        }
        Ok(())
    }

    pub fn handler(&self) -> Arc<Handler<T>> {
        self.handler.clone()
    }
}
