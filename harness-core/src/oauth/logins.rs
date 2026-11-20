// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Andrea Cervesato <andrea.cervesato@linux.dev>

//! The OAuth logins a client authenticates with.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use genai::adapter::AdapterKind;
use genai::resolver::{self, AuthData, AuthResolver};
use genai::ModelIden;

use super::{OAuthProvider, OAuthStore};
use crate::Error;

type ResolverFuture = Pin<Box<dyn Future<Output = resolver::Result<Option<AuthData>>> + Send>>;

/// Logins in use, each with the API base URL it talks to.
#[derive(Debug, Clone)]
pub struct OAuthLogins {
    store: Arc<OAuthStore>,
    logins: Arc<[(OAuthProvider, Option<String>)]>,
}

impl OAuthLogins {
    /// Use `logins` (provider, endpoint override) from `store`; `None` when
    /// there is none.
    pub(crate) fn new(
        store: Arc<OAuthStore>,
        logins: Vec<(OAuthProvider, Option<String>)>,
    ) -> Option<Self> {
        (!logins.is_empty()).then(|| Self {
            store,
            logins: logins.into(),
        })
    }

    /// Whether a login authenticates `kind`.
    pub fn covers(&self, kind: AdapterKind) -> bool {
        self.find(kind).is_some()
    }

    fn find(&self, kind: AdapterKind) -> Option<(OAuthProvider, Option<&str>)> {
        self.logins
            .iter()
            .find(|(p, _)| p.adapter() == kind)
            .map(|(p, endpoint)| (*p, endpoint.as_deref()))
    }

    /// `genai` authentication for `kind`, or `None` without a login.
    pub async fn auth_data(&self, kind: AdapterKind) -> Result<Option<AuthData>, Error> {
        match self.find(kind) {
            Some((provider, endpoint)) => self.store.auth_data(provider, endpoint).await.map(Some),
            None => Ok(None),
        }
    }

    /// Text the login for `kind` requires as the first system prompt block.
    pub fn system_prefix(&self, kind: AdapterKind) -> Option<&'static str> {
        let (provider, _) = self.find(kind)?;
        self.store.system_prefix(provider)
    }

    /// Models available to the login for `kind`.
    pub async fn models(&self, kind: AdapterKind) -> Result<Vec<String>, Error> {
        let (provider, endpoint) = self
            .find(kind)
            .ok_or_else(|| format!("no login for {kind}"))?;
        self.store.models(provider, endpoint).await
    }

    /// Resolver authenticating requests with these logins.
    pub(crate) fn resolver(self) -> AuthResolver {
        AuthResolver::from_resolver_async_fn(move |model: ModelIden| -> ResolverFuture {
            let logins = self.clone();
            Box::pin(async move {
                logins
                    .auth_data(model.adapter_kind)
                    .await
                    .map_err(|e| resolver::Error::Custom(e.to_string()))
            })
        })
    }
}
