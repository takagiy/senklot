use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::os::unix::net;
use std::path::{Path, PathBuf};

pub trait MutDict<V> {
    fn set(&mut self, key: &str, value: V);
}

impl<V> MutDict<V> for HashMap<String, V> {
    fn set(&mut self, key: &str, value: V) {
        match self.get_mut(key) {
            Some(dist) => {
                *dist = value;
            }
            None => {
                self.insert(key.to_owned(), value);
            }
        }
    }
}

pub trait Optional<T> {
    fn into_option(self) -> Option<T>;
}

pub trait OptionCond<T> {
    fn and_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool;
    fn or_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool;
    fn and_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool;
    fn or_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool;
}

impl<T, O: Optional<T>> OptionCond<T> for O {
    fn and_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool {
        self.into_option().and_then(pred).unwrap_or(false)
    }
    fn or_if_flat<F: FnOnce(T) -> Option<bool>>(self, pred: F) -> bool {
        self.into_option().and_then(pred).unwrap_or(true)
    }
    fn and_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool {
        self.into_option().map(pred).unwrap_or(false)
    }
    fn or_if<F: FnOnce(T) -> bool>(self, pred: F) -> bool {
        self.into_option().map(pred).unwrap_or(true)
    }
}

impl<T> Optional<T> for Option<T> {
    fn into_option(self) -> Self {
        self
    }
}

impl<T, E> Optional<T> for Result<T, E> {
    fn into_option(self) -> Option<T> {
        self.ok()
    }
}

pub struct SocketPath {
    path: PathBuf,
}

impl SocketPath {
    pub fn bind<P: AsRef<Path>>(path: P) -> Result<(SocketPath, net::UnixListener)> {
        let _ = fs::remove_file(path.as_ref());
        let listener = net::UnixListener::bind(path.as_ref())?;
        Ok((
            SocketPath {
                path: path.as_ref().to_path_buf(),
            },
            listener,
        ))
    }

    pub fn allow_write(&self) -> Result<()> {
        let mut permissions = fs::metadata(&self.path)?.permissions();
        permissions.set_readonly(false);
        let ok = fs::set_permissions(&self.path, permissions)?;
        Ok(ok)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        fs::remove_file(&self.path).expect("Unable to remove the socket");
    }
}
