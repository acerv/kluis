// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! Login credentials saved in `oauth.toml`.
//!
//! The file is only written by us: atomically, with mode 0600, and under an
//! exclusive lock on `oauth.toml.lock` so concurrent processes never use a
//! refresh token twice.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use genai::resolver::AuthData;
use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};

use super::anthropic::Anthropic;
use super::flow::{self, Login, Token};
use super::{OAuthProvider, Provider, Tokens};
use crate::{private_file, Error};

/// Maximum size of an `oauth.toml` file (64 KiB).
const MAX_OAUTH_BYTES: usize = 64 * 1024;

/// Contents of `oauth.toml`: one table per logged-in provider.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) anthropic: Option<Tokens>,
}

/// Run `$body` with `$p` bound to the [`Provider`] for `$id`.
macro_rules! with_provider {
    ($store:expr, $id:expr, |$p:ident| $body:expr) => {
        match $id {
            OAuthProvider::Anthropic => {
                let $p = &$store.anthropic;
                $body
            }
        }
    };
}

/// Saved logins, refreshed on use.
#[derive(Debug)]
pub struct OAuthStore {
    path: PathBuf,
    file: Mutex<OAuthFile>,
    /// Serializes refreshes within this process.
    refresh: tokio::sync::Mutex<()>,
    anthropic: Anthropic,
}

impl OAuthStore {
    /// Load the logins saved at `path`; a missing file means no login.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let file = read(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            refresh: tokio::sync::Mutex::new(()),
            anthropic: Anthropic::default(),
        })
    }

    /// Use custom Anthropic login settings.
    pub fn with_anthropic(mut self, anthropic: Anthropic) -> Self {
        self.anthropic = anthropic;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_logged_in(&self, provider: OAuthProvider) -> bool {
        with_provider!(self, provider, |p| p.slot(&mut self.cached()).is_some())
    }

    /// Providers with a saved login.
    pub fn providers(&self) -> Vec<OAuthProvider> {
        OAuthProvider::ALL
            .iter()
            .copied()
            .filter(|&p| self.is_logged_in(p))
            .collect()
    }

    /// Begin a login to `provider`; pass the resulting token to
    /// [`Self::finish_login`].
    pub async fn start_login(&self, provider: OAuthProvider) -> Result<Login, Error> {
        flow::start(with_provider!(self, provider, |p| p.flow())).await
    }

    /// Save the login `token` obtained for `provider`.
    pub async fn finish_login(&self, provider: OAuthProvider, token: &Token) -> Result<(), Error> {
        with_provider!(self, provider, |p| {
            let creds = p.login(token).await?;
            self.update(|file| *p.slot(file) = Some(creds)).await
        })
    }

    /// Forget the login to `provider`; returns whether there was one.
    pub async fn logout(&self, provider: OAuthProvider) -> Result<bool, Error> {
        with_provider!(self, provider, |p| {
            self.update(|file| p.slot(file).take().is_some()).await
        })
    }

    /// A valid access token for `provider`, refreshed when about to expire.
    pub async fn access_token(&self, provider: OAuthProvider) -> Result<String, Error> {
        with_provider!(self, provider, |p| {
            let creds = self.credentials(p).await?;
            Ok(p.access_token(&creds).to_owned())
        })
    }

    /// `genai` authentication for `provider`, refreshed when about to expire;
    /// `endpoint` overrides the provider's API base URL.
    pub async fn auth_data(
        &self,
        provider: OAuthProvider,
        endpoint: Option<&str>,
    ) -> Result<AuthData, Error> {
        with_provider!(self, provider, |p| {
            let creds = self.credentials(p).await?;
            Ok(p.auth(&creds, endpoint))
        })
    }

    /// Models available to the login to `provider`; `endpoint` overrides the
    /// provider's API base URL.
    pub async fn models(
        &self,
        provider: OAuthProvider,
        endpoint: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        with_provider!(self, provider, |p| {
            let creds = self.credentials(p).await?;
            p.models(&creds, endpoint).await
        })
    }

    /// Credentials of `p`, refreshed when about to expire.
    async fn credentials<P: Provider>(&self, p: &P) -> Result<P::Credentials, Error> {
        let not_logged_in = || format!("not logged in to {}", P::ID).into();
        let creds = p
            .slot(&mut self.cached())
            .clone()
            .ok_or_else(not_logged_in)?;
        if !p.expires_soon(&creds) {
            return Ok(creds);
        }

        let _guard = self.refresh.lock().await;
        let _lock = self.lock().await?;
        // Another task or process may have refreshed or logged out already.
        let mut file = read(&self.path)?;
        let stored = p.slot(&mut file).clone();
        let creds = match stored {
            Some(creds) if p.expires_soon(&creds) => {
                let fresh = p.refresh(&creds).await?;
                *p.slot(&mut file) = Some(fresh.clone());
                write(&self.path, &file)?;
                Some(fresh)
            }
            stored => stored,
        };
        *self.cached() = file;
        creds.ok_or_else(not_logged_in)
    }

    /// Apply `f` to the file on disk and save it, under the file lock.
    async fn update<R>(&self, f: impl FnOnce(&mut OAuthFile) -> R) -> Result<R, Error> {
        let _lock = self.lock().await?;
        let mut file = read(&self.path)?;
        let out = f(&mut file);
        write(&self.path, &file)?;
        *self.cached() = file;
        Ok(out)
    }

    fn cached(&self) -> MutexGuard<'_, OAuthFile> {
        self.file.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the exclusive lock shared by all processes; released on drop.
    async fn lock(&self) -> Result<File, Error> {
        let path = sibling(&self.path, ".lock");
        tokio::task::spawn_blocking(move || -> Result<File, Error> {
            if let Some(dir) = path.parent() {
                DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            }
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            flock(&file, FlockOperation::LockExclusive)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(file)
        })
        .await?
    }
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    name.into()
}

fn read(path: &Path) -> Result<OAuthFile, Error> {
    let Some(raw) = private_file::read(path, MAX_OAUTH_BYTES)? else {
        return Ok(OAuthFile::default());
    };
    toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()).into())
}

/// Atomically replace `path` with `file`, or remove it when empty.
fn write(path: &Path, file: &OAuthFile) -> Result<(), Error> {
    let error = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let raw = toml::to_string(file).map_err(|e| error(&e))?;
    if raw.trim().is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(error(&e).into()),
            _ => Ok(()),
        };
    }
    let tmp = sibling(path, ".tmp");
    let _ = fs::remove_file(&tmp);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| error(&e))?;
    out.write_all(raw.as_bytes())
        .and_then(|()| out.sync_all())
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            error(&e).into()
        })
}
