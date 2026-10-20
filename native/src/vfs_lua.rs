//! `remuda.vfs`: read-only Lua access to the cluster file store (#742).
//! Every call answers from this node's index and blob cache and never touches
//! the network, so it is safe on the daemon's Lua thread.

use crate::cluster::vfs::{VfsError, VfsErrorKind, VfsStore};
use mlua::{IntoLua, Lua, Table, Value};
use remuda_core::vfs::VfsEntry;

fn entry_table(lua: &Lua, store: &VfsStore, entry: &VfsEntry) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("path", entry.path.as_str())?;
    table.set("version", entry.version)?;
    table.set("sha256", entry.sha256.as_str())?;
    table.set("size", entry.size)?;
    table.set("mtime", entry.mtime)?;
    table.set("replicas", entry.replicas)?;
    table.set("writer", entry.writer.as_str())?;
    table.set("local", store.has_blob(&entry.sha256, entry.size))?;
    table.set("synced_at", store.synced_at())?;
    Ok(table)
}

fn not_found(path: &str) -> VfsError {
    VfsError::new(VfsErrorKind::NotFound, format!("no file at {path}"))
}

fn stat(lua: &Lua, uri: &str) -> Result<Table, mlua::Result<VfsError>> {
    let store = VfsStore::open_default().map_err(Ok)?;
    let uri = store.parse_uri(uri).map_err(Ok)?;
    let index = store.load_index().map_err(Ok)?;
    let entry = index
        .live(&uri.path)
        .ok_or_else(|| Ok(not_found(&uri.path)))?;
    entry_table(lua, &store, entry).map_err(Err)
}

fn read(lua: &Lua, uri: &str) -> Result<mlua::LuaString, mlua::Result<VfsError>> {
    let store = VfsStore::open_default().map_err(Ok)?;
    let uri = store.parse_uri(uri).map_err(Ok)?;
    let index = store.load_index().map_err(Ok)?;
    let entry = index
        .live(&uri.path)
        .ok_or_else(|| Ok(not_found(&uri.path)))?;
    let bytes = store.read_blob(&entry.sha256).map_err(Ok)?;
    lua.create_string(bytes).map_err(Err)
}

fn list(lua: &Lua, prefix: &str) -> Result<Table, mlua::Result<VfsError>> {
    let store = VfsStore::open_default().map_err(Ok)?;
    let prefix = crate::vfs_cli::list_prefix(&store, prefix).map_err(Ok)?;
    let index = store.load_index().map_err(Ok)?;
    let rows = lua.create_table().map_err(Err)?;
    for (position, entry) in index.list(&prefix).into_iter().enumerate() {
        rows.set(position + 1, entry_table(lua, &store, entry).map_err(Err)?)
            .map_err(Err)?;
    }
    Ok(rows)
}

/// `value, nil` on success; `nil, "code: detail"` on a store failure. Lua
/// usage errors still raise.
fn answer<T: IntoLua>(
    lua: &Lua,
    result: Result<T, mlua::Result<VfsError>>,
) -> mlua::Result<(Value, Value)> {
    match result {
        Ok(value) => Ok((value.into_lua(lua)?, Value::Nil)),
        Err(Ok(error)) => Ok((Value::Nil, error.reason().into_lua(lua)?)),
        Err(Err(error)) => Err(error),
    }
}

pub fn bindings(lua: &Lua) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "stat",
        lua.create_function(|lua, uri: String| answer(lua, stat(lua, &uri)))?,
    )?;
    table.set(
        "read",
        lua.create_function(|lua, uri: String| answer(lua, read(lua, &uri)))?,
    )?;
    table.set(
        "list",
        lua.create_function(|lua, prefix: String| answer(lua, list(lua, &prefix)))?,
    )?;
    Ok(table)
}
