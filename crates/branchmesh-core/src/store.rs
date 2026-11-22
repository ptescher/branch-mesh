use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::media::MediaRef;
use crate::object::Object;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Immutable content-addressed objects: hash bytes -> canonical JSON.
const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects");
/// Raw media bytes (images): hash bytes -> bytes. Metadata lives in the
/// `media` document namespace.
const MEDIA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("media");
/// Mutable documents keyed by "<namespace>/<key>".
const DOCS: TableDefinition<&str, &[u8]> = TableDefinition::new("docs");

/// Well-known document namespaces.
pub mod ns {
    pub const REFS: &str = "refs";
    pub const EXECUTIONS: &str = "executions";
    pub const JOURNAL: &str = "journal";
    pub const MAILBOX: &str = "mailbox";
    pub const PERSONAS: &str = "personas";
    pub const MERGE_REQUESTS: &str = "merge_requests";
    pub const META: &str = "meta";
    pub const MEDIA: &str = "media";
}

/// Embedded storage engine (redb). Cheap to clone.
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

fn doc_key(namespace: &str, key: &str) -> String {
    format!("{namespace}/{key}")
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| Error::Storage(e.to_string()))?;
            }
        }
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        {
            tx.open_table(OBJECTS)?;
            tx.open_table(DOCS)?;
            tx.open_table(MEDIA)?;
        }
        tx.commit()?;
        Ok(Store { db: Arc::new(db) })
    }

    // ---------------------------------------------------------------- objects

    pub fn put_object(&self, obj: &Object) -> Result<Hash> {
        Ok(self.put_objects(std::slice::from_ref(obj))?[0])
    }

    /// Write several objects in a single transaction. Already-present
    /// objects are skipped (content addressing makes writes idempotent).
    pub fn put_objects(&self, objs: &[Object]) -> Result<Vec<Hash>> {
        let tx = self.db.begin_write()?;
        let mut hashes = Vec::with_capacity(objs.len());
        {
            let mut table = tx.open_table(OBJECTS)?;
            for obj in objs {
                let bytes = obj.encode();
                let hash = Hash::of(&bytes);
                if table.get(hash.as_bytes())?.is_none() {
                    table.insert(hash.as_bytes(), bytes.as_slice())?;
                }
                hashes.push(hash);
            }
        }
        tx.commit()?;
        Ok(hashes)
    }

    pub fn get_object(&self, hash: &Hash) -> Result<Object> {
        self.try_get_object(hash)?
            .ok_or_else(|| Error::ObjectNotFound(hash.to_hex()))
    }

    pub fn try_get_object(&self, hash: &Hash) -> Result<Option<Object>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(OBJECTS)?;
        match table.get(hash.as_bytes())? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn object_count(&self) -> Result<u64> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(OBJECTS)?;
        Ok(table.len()?)
    }

    // ---------------------------------------------------------------- media

    /// Store media bytes (idempotent) and their metadata. Callers validate
    /// the bytes first (see [`crate::media::validate_image`]).
    pub fn put_media(&self, bytes: &[u8], media_type: &str) -> Result<MediaRef> {
        let hash = Hash::of(bytes);
        let media = MediaRef { hash: hash.to_hex(), media_type: media_type.into(), size: bytes.len() as u64 };
        let meta = serde_json::to_vec(&media)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(MEDIA)?;
            if table.get(hash.as_bytes())?.is_none() {
                table.insert(hash.as_bytes(), bytes)?;
            }
            let mut docs = tx.open_table(DOCS)?;
            docs.insert(doc_key(ns::MEDIA, &media.hash).as_str(), meta.as_slice())?;
        }
        tx.commit()?;
        Ok(media)
    }

    /// Metadata of stored media (`hash` as hex).
    pub fn media_info(&self, hash: &str) -> Result<Option<MediaRef>> {
        self.get_doc(ns::MEDIA, hash)
    }

    /// Bytes of stored media (`hash` as hex).
    pub fn get_media(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let hash = Hash::from_hex(hash)?;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(MEDIA)?;
        Ok(table.get(hash.as_bytes())?.map(|v| v.value().to_vec()))
    }

    // ---------------------------------------------------------------- documents

    pub fn put_doc<T: Serialize>(&self, namespace: &str, key: &str, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(DOCS)?;
            table.insert(doc_key(namespace, key).as_str(), bytes.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_doc<T: DeserializeOwned>(&self, namespace: &str, key: &str) -> Result<Option<T>> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DOCS)?;
        match table.get(doc_key(namespace, key).as_str())? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn delete_doc(&self, namespace: &str, key: &str) -> Result<bool> {
        let tx = self.db.begin_write()?;
        let existed;
        {
            let mut table = tx.open_table(DOCS)?;
            existed = table.remove(doc_key(namespace, key).as_str())?.is_some();
        }
        tx.commit()?;
        Ok(existed)
    }

    /// List documents in a namespace whose key starts with `prefix`, in key order.
    pub fn list_docs<T: DeserializeOwned>(&self, namespace: &str, prefix: &str) -> Result<Vec<(String, T)>> {
        let full_prefix = doc_key(namespace, prefix);
        let strip = namespace.len() + 1;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DOCS)?;
        let mut out = Vec::new();
        for item in table.range(full_prefix.as_str()..)? {
            let (k, v) = item?;
            let key = k.value();
            if !key.starts_with(&full_prefix) {
                break;
            }
            let value: T = serde_json::from_slice(v.value())?;
            out.push((key[strip..].to_string(), value));
        }
        Ok(out)
    }

    /// Atomic read-modify-write of a single document. The closure receives the
    /// current value (if any) and returns the new value (`None` deletes).
    /// Runs inside one write transaction, so it doubles as compare-and-swap.
    pub fn update_doc<T, F, R>(&self, namespace: &str, key: &str, f: F) -> Result<R>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce(Option<T>) -> Result<(Option<T>, R)>,
    {
        let k = doc_key(namespace, key);
        let tx = self.db.begin_write()?;
        let result;
        {
            let mut table = tx.open_table(DOCS)?;
            let current: Option<T> = match table.get(k.as_str())? {
                Some(v) => Some(serde_json::from_slice(v.value())?),
                None => None,
            };
            let (next, r) = f(current)?;
            match next {
                Some(v) => {
                    let bytes = serde_json::to_vec(&v)?;
                    table.insert(k.as_str(), bytes.as_slice())?;
                }
                None => {
                    table.remove(k.as_str())?;
                }
            }
            result = r;
        }
        tx.commit()?;
        Ok(result)
    }

    pub fn count_docs(&self, namespace: &str) -> Result<usize> {
        Ok(self.list_docs::<serde_json::Value>(namespace, "")?.len())
    }
}
