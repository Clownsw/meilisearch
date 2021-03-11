use std::collections::HashMap;
use std::fs::{create_dir_all, remove_dir_all, File};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_stream::stream;
use chrono::{DateTime, Utc};
use futures::pin_mut;
use futures::stream::StreamExt;
use heed::{
    types::{ByteSlice, SerdeBincode},
    Database, Env, EnvOpenOptions,
};
use log::debug;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::{mpsc, oneshot, RwLock}};
use tokio::task::spawn_blocking;
use uuid::Uuid;

use super::get_arc_ownership_blocking;
use super::update_handler::UpdateHandler;
use crate::index::UpdateResult as UResult;
use crate::index::{Document, Index, SearchQuery, SearchResult, Settings};
use crate::index_controller::{
    updates::{Failed, Processed, Processing},
    UpdateMeta,
};
use crate::option::IndexerOpts;

pub type Result<T> = std::result::Result<T, IndexError>;
type AsyncMap<K, V> = Arc<RwLock<HashMap<K, V>>>;
type UpdateResult = std::result::Result<Processed<UpdateMeta, UResult>, Failed<UpdateMeta, String>>;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IndexMeta {
    uuid: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    primary_key: Option<String>,
}

enum IndexMsg {
    CreateIndex {
        uuid: Uuid,
        primary_key: Option<String>,
        ret: oneshot::Sender<Result<IndexMeta>>,
    },
    Update {
        meta: Processing<UpdateMeta>,
        data: std::fs::File,
        ret: oneshot::Sender<Result<UpdateResult>>,
    },
    Search {
        uuid: Uuid,
        query: SearchQuery,
        ret: oneshot::Sender<anyhow::Result<SearchResult>>,
    },
    Settings {
        uuid: Uuid,
        ret: oneshot::Sender<Result<Settings>>,
    },
    Documents {
        uuid: Uuid,
        attributes_to_retrieve: Option<Vec<String>>,
        offset: usize,
        limit: usize,
        ret: oneshot::Sender<Result<Vec<Document>>>,
    },
    Document {
        uuid: Uuid,
        attributes_to_retrieve: Option<Vec<String>>,
        doc_id: String,
        ret: oneshot::Sender<Result<Document>>,
    },
    Delete {
        uuid: Uuid,
        ret: oneshot::Sender<Result<()>>,
    },
    GetMeta {
        uuid: Uuid,
        ret: oneshot::Sender<Result<Option<IndexMeta>>>,
    },
}

struct IndexActor<S> {
    read_receiver: Option<mpsc::Receiver<IndexMsg>>,
    write_receiver: Option<mpsc::Receiver<IndexMsg>>,
    update_handler: Arc<UpdateHandler>,
    store: S,
}

#[derive(Error, Debug)]
pub enum IndexError {
    #[error("error with index: {0}")]
    Error(#[from] anyhow::Error),
    #[error("index already exists")]
    IndexAlreadyExists,
    #[error("Index doesn't exists")]
    UnexistingIndex,
    #[error("Heed error: {0}")]
    HeedError(#[from] heed::Error),
}

#[async_trait::async_trait]
trait IndexStore {
    async fn create_index(&self, uuid: Uuid, primary_key: Option<String>) -> Result<IndexMeta>;
    async fn update_index<R, F>(&self, uuid: Uuid, f: F) -> Result<R>
    where
        F: FnOnce(Index) -> Result<R> + Send + Sync + 'static,
        R: Sync + Send + 'static;
    async fn get(&self, uuid: Uuid) -> Result<Option<Index>>;
    async fn delete(&self, uuid: Uuid) -> Result<Option<Index>>;
    async fn get_meta(&self, uuid: Uuid) -> Result<Option<IndexMeta>>;
}

impl<S: IndexStore + Sync + Send> IndexActor<S> {
    fn new(
        read_receiver: mpsc::Receiver<IndexMsg>,
        write_receiver: mpsc::Receiver<IndexMsg>,
        store: S,
    ) -> Result<Self> {
        let options = IndexerOpts::default();
        let update_handler =
            UpdateHandler::new(&options).map_err(|e| IndexError::Error(e.into()))?;
        let update_handler = Arc::new(update_handler);
        let read_receiver = Some(read_receiver);
        let write_receiver = Some(write_receiver);
        Ok(Self {
            read_receiver,
            write_receiver,
            store,
            update_handler,
        })
    }

    /// `run` poll the write_receiver and read_receiver concurrently, but while messages send
    /// through the read channel are processed concurrently, the messages sent through the write
    /// channel are processed one at a time.
    async fn run(mut self) {
        let mut read_receiver = self
            .read_receiver
            .take()
            .expect("Index Actor must have a inbox at this point.");

        let read_stream = stream! {
            loop {
                match read_receiver.recv().await {
                    Some(msg) => yield msg,
                    None => break,
                }
            }
        };

        let mut write_receiver = self
            .write_receiver
            .take()
            .expect("Index Actor must have a inbox at this point.");

        let write_stream = stream! {
            loop {
                match write_receiver.recv().await {
                    Some(msg) => yield msg,
                    None => break,
                }
            }
        };

        pin_mut!(write_stream);
        pin_mut!(read_stream);

        let fut1 = read_stream.for_each_concurrent(Some(10), |msg| self.handle_message(msg));
        let fut2 = write_stream.for_each_concurrent(Some(1), |msg| self.handle_message(msg));

        let fut1: Box<dyn Future<Output = ()> + Unpin + Send> = Box::new(fut1);
        let fut2: Box<dyn Future<Output = ()> + Unpin + Send> = Box::new(fut2);

        tokio::join!(fut1, fut2);
    }

    async fn handle_message(&self, msg: IndexMsg) {
        use IndexMsg::*;
        match msg {
            CreateIndex {
                uuid,
                primary_key,
                ret,
            } => {
                let _ = ret.send(self.handle_create_index(uuid, primary_key).await);
            }
            Update { ret, meta, data } => {
                let _ = ret.send(self.handle_update(meta, data).await);
            }
            Search { ret, query, uuid } => {
                let _ = ret.send(self.handle_search(uuid, query).await);
            }
            Settings { ret, uuid } => {
                let _ = ret.send(self.handle_settings(uuid).await);
            }
            Documents {
                ret,
                uuid,
                attributes_to_retrieve,
                offset,
                limit,
            } => {
                let _ = ret.send(
                    self.handle_fetch_documents(uuid, offset, limit, attributes_to_retrieve)
                        .await,
                );
            }
            Document {
                uuid,
                attributes_to_retrieve,
                doc_id,
                ret,
            } => {
                let _ = ret.send(
                    self.handle_fetch_document(uuid, doc_id, attributes_to_retrieve)
                        .await,
                );
            }
            Delete { uuid, ret } => {
                let _ = ret.send(self.handle_delete(uuid).await);
            }
            GetMeta { uuid, ret } => {
                let _ = ret.send(self.handle_get_meta(uuid).await);
            }
        }
    }

    async fn handle_search(&self, uuid: Uuid, query: SearchQuery) -> anyhow::Result<SearchResult> {
        let index = self
            .store
            .get(uuid)
            .await?
            .ok_or(IndexError::UnexistingIndex)?;
        spawn_blocking(move || index.perform_search(query)).await?
    }

    async fn handle_create_index(
        &self,
        uuid: Uuid,
        primary_key: Option<String>,
    ) -> Result<IndexMeta> {
        self.store.create_index(uuid, primary_key).await
    }

    async fn handle_update(
        &self,
        meta: Processing<UpdateMeta>,
        data: File,
    ) -> Result<UpdateResult> {
        debug!("Processing update {}", meta.id());
        let uuid = meta.index_uuid().clone();
        let update_handler = self.update_handler.clone();
        let handle = self
            .store
            .update_index(uuid, |index| {
                let handle =
                    spawn_blocking(move || update_handler.handle_update(meta, data, index));
                Ok(handle)
            })
            .await?;

        handle.await.map_err(|e| IndexError::Error(e.into()))
    }

    async fn handle_settings(&self, uuid: Uuid) -> Result<Settings> {
        let index = self
            .store
            .get(uuid)
            .await?
            .ok_or(IndexError::UnexistingIndex)?;
        spawn_blocking(move || index.settings().map_err(|e| IndexError::Error(e)))
            .await
            .map_err(|e| IndexError::Error(e.into()))?
    }

    async fn handle_fetch_documents(
        &self,
        uuid: Uuid,
        offset: usize,
        limit: usize,
        attributes_to_retrieve: Option<Vec<String>>,
    ) -> Result<Vec<Document>> {
        let index = self
            .store
            .get(uuid)
            .await?
            .ok_or(IndexError::UnexistingIndex)?;
        spawn_blocking(move || {
            index
                .retrieve_documents(offset, limit, attributes_to_retrieve)
                .map_err(|e| IndexError::Error(e))
        })
        .await
        .map_err(|e| IndexError::Error(e.into()))?
    }

    async fn handle_fetch_document(
        &self,
        uuid: Uuid,
        doc_id: String,
        attributes_to_retrieve: Option<Vec<String>>,
    ) -> Result<Document> {
        let index = self
            .store
            .get(uuid)
            .await?
            .ok_or(IndexError::UnexistingIndex)?;
        spawn_blocking(move || {
            index
                .retrieve_document(doc_id, attributes_to_retrieve)
                .map_err(|e| IndexError::Error(e))
        })
        .await
        .map_err(|e| IndexError::Error(e.into()))?
    }

    async fn handle_delete(&self, uuid: Uuid) -> Result<()> {
        let index = self.store.delete(uuid).await?;

        if let Some(index) = index {
            tokio::task::spawn(async move {
                let index = index.0;
                let store = get_arc_ownership_blocking(index).await;
                spawn_blocking(move || {
                    store.prepare_for_closing().wait();
                    debug!("Index closed");
                });
            });
        }

        Ok(())
    }

    async fn handle_get_meta(&self, uuid: Uuid) -> Result<Option<IndexMeta>> {
        let result = self.store.get_meta(uuid).await?;
        Ok(result)
    }
}

#[derive(Clone)]
pub struct IndexActorHandle {
    read_sender: mpsc::Sender<IndexMsg>,
    write_sender: mpsc::Sender<IndexMsg>,
}

impl IndexActorHandle {
    pub fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let (read_sender, read_receiver) = mpsc::channel(100);
        let (write_sender, write_receiver) = mpsc::channel(100);

        let store = HeedIndexStore::new(path)?;
        let actor = IndexActor::new(read_receiver, write_receiver, store)?;
        tokio::task::spawn(actor.run());
        Ok(Self {
            read_sender,
            write_sender,
        })
    }

    pub async fn create_index(&self, uuid: Uuid, primary_key: Option<String>) -> Result<IndexMeta> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::CreateIndex {
            ret,
            uuid,
            primary_key,
        };
        let _ = self.read_sender.send(msg).await;
        receiver.await.expect("IndexActor has been killed")
    }

    pub async fn update(
        &self,
        meta: Processing<UpdateMeta>,
        data: std::fs::File,
    ) -> anyhow::Result<UpdateResult> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::Update { ret, meta, data };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }

    pub async fn search(&self, uuid: Uuid, query: SearchQuery) -> Result<SearchResult> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::Search { uuid, query, ret };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }

    pub async fn settings(&self, uuid: Uuid) -> Result<Settings> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::Settings { uuid, ret };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }

    pub async fn documents(
        &self,
        uuid: Uuid,
        offset: usize,
        limit: usize,
        attributes_to_retrieve: Option<Vec<String>>,
    ) -> Result<Vec<Document>> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::Documents {
            uuid,
            ret,
            offset,
            attributes_to_retrieve,
            limit,
        };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }

    pub async fn document(
        &self,
        uuid: Uuid,
        doc_id: String,
        attributes_to_retrieve: Option<Vec<String>>,
    ) -> Result<Document> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::Document {
            uuid,
            ret,
            doc_id,
            attributes_to_retrieve,
        };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }

    pub async fn delete(&self, uuid: Uuid) -> Result<()> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::Delete { uuid, ret };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }

    pub async fn get_index_meta(&self, uuid: Uuid) -> Result<Option<IndexMeta>> {
        let (ret, receiver) = oneshot::channel();
        let msg = IndexMsg::GetMeta { uuid, ret };
        let _ = self.read_sender.send(msg).await;
        Ok(receiver.await.expect("IndexActor has been killed")?)
    }
}

struct HeedIndexStore {
    env: Env,
    db: Database<ByteSlice, SerdeBincode<IndexMeta>>,
    index_store: AsyncMap<Uuid, Index>,
    path: PathBuf,
}

impl HeedIndexStore {
    fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut options = EnvOpenOptions::new();
        options.map_size(1_073_741_824); //1GB
        let path = path.as_ref().join("indexes/");
        create_dir_all(&path)?;
        let env = options.open(&path)?;
        let db = env.create_database(None)?;
        let index_store = Arc::new(RwLock::new(HashMap::new()));
        Ok(Self {
            env,
            db,
            index_store,
            path,
        })
    }
}

#[async_trait::async_trait]
impl IndexStore for HeedIndexStore {
    async fn create_index(&self, uuid: Uuid, primary_key: Option<String>) -> Result<IndexMeta> {
        let path = self.path.join(format!("index-{}", uuid));

        if path.exists() {
            return Err(IndexError::IndexAlreadyExists);
        }

        let env = self.env.clone();
        let db = self.db.clone();
        let (index, meta) = spawn_blocking(move || -> Result<(Index, IndexMeta)> {
            let now = Utc::now();
            let meta = IndexMeta {
                uuid: uuid.clone(),
                created_at: now.clone(),
                updated_at: now,
                primary_key,
            };
            let mut txn = env.write_txn()?;
            db.put(&mut txn, uuid.as_bytes(), &meta)?;
            txn.commit()?;

            let index = open_index(&path, 4096 * 100_000)?;

            Ok((index, meta))
        })
        .await
        .expect("thread died")?;

        self.index_store.write().await.insert(uuid.clone(), index);

        Ok(meta)
    }

    async fn update_index<R, F>(&self, uuid: Uuid, f: F) -> Result<R>
    where
        F: FnOnce(Index) -> Result<R> + Send + Sync + 'static,
        R: Sync + Send + 'static,
    {
        let guard = self.index_store.read().await;
        let index = match guard.get(&uuid) {
            Some(index) => index.clone(),
            None => {
                drop(guard);
                self.create_index(uuid.clone(), None).await?;
                self.index_store
                    .read()
                    .await
                    .get(&uuid)
                    .expect("Index should exist")
                    .clone()
            }
        };

        let env = self.env.clone();
        let db = self.db.clone();
        spawn_blocking(move || {
            let mut txn = env.write_txn()?;
            let mut meta = db.get(&txn, uuid.as_bytes())?.expect("unexisting index");
            match f(index) {
                Ok(r) => {
                    meta.updated_at = Utc::now();
                    db.put(&mut txn, uuid.as_bytes(), &meta)?;
                    txn.commit()?;
                    Ok(r)
                }
                Err(e) => Err(e),
            }
        })
        .await
        .expect("thread died")
    }

    async fn get(&self, uuid: Uuid) -> Result<Option<Index>> {
        let guard = self.index_store.read().await;
        match guard.get(&uuid) {
            Some(index) => Ok(Some(index.clone())),
            None => {
                // drop the guard here so we can perform the write after without deadlocking;
                drop(guard);
                let path = self.path.join(format!("index-{}", uuid));
                if !path.exists() {
                    return Ok(None);
                }

                // TODO: set this info from the database
                let index = spawn_blocking(|| open_index(path, 4096 * 100_000))
                    .await
                    .expect("thread died")?;
                self.index_store
                    .write()
                    .await
                    .insert(uuid.clone(), index.clone());
                println!("here");
                Ok(Some(index))
            }
        }
    }

    async fn delete(&self, uuid: Uuid) -> Result<Option<Index>> {
        let env = self.env.clone();
        let db = self.db.clone();
        let db_path = self.path.join(format!("index-{}", uuid));
        spawn_blocking(move || -> Result<()> {
            let mut txn = env.write_txn()?;
            db.delete(&mut txn, uuid.as_bytes())?;
            txn.commit()?;
            remove_dir_all(db_path).unwrap();
            Ok(())
        })
        .await
        .expect("thread died")?;
        let index = self.index_store.write().await.remove(&uuid);
        Ok(index)
    }

    async fn get_meta(&self, uuid: Uuid) -> Result<Option<IndexMeta>> {
        let env = self.env.clone();
        let db = self.db.clone();
        spawn_blocking(move || {
            let txn = env.read_txn()?;
            let meta = db.get(&txn, uuid.as_bytes())?;
            Ok(meta)
        })
        .await
        .expect("thread died")
    }
}

fn open_index(path: impl AsRef<Path>, size: usize) -> Result<Index> {
    create_dir_all(&path).expect("can't create db");
    let mut options = EnvOpenOptions::new();
    options.map_size(size);
    let index = milli::Index::new(options, &path).map_err(|e| IndexError::Error(e))?;
    Ok(Index(Arc::new(index)))
}
