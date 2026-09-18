use std::collections::HashMap;

use uuid::Uuid;

use oneclient_common::paths;
use oneclient_events::EventBus;

use crate::data::{AccountKind, MinecraftAccount};
use crate::error::{AuthError, AuthResult};
use crate::offline::{offline_account, validate_offline_username};

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct CredentialsStore {
    pub users: HashMap<Uuid, MinecraftAccount>,
    pub default_user: Option<Uuid>,
}

impl CredentialsStore {
    #[tracing::instrument(level = "debug")]
    pub async fn load() -> AuthResult<Self> {
        let path = paths::auth_file()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        match polyio::read_json(&path).await {
            Ok(store) => Ok(store),
            Err(err) => {
                tracing::warn!("failed to read auth file: {err}");
                Ok(Self::default())
            }
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn save(&self) -> AuthResult<()> {
        let path = paths::auth_file()?;

        polyio::write_json_atomic(&path, self).await?;
        Ok(())
    }

    pub fn list_accounts(&self) -> Vec<MinecraftAccount> {
        self.users
            .values()
            .filter(|account| account.kind == AccountKind::Offline)
            .cloned()
            .collect()
    }

    pub fn get_account(&self, id: Uuid) -> Option<&MinecraftAccount> {
        self.users
            .get(&id)
            .filter(|account| account.kind == AccountKind::Offline)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(username = %account.username))]
    pub async fn commit_account(
        &mut self,
        account: MinecraftAccount,
        events: &EventBus,
    ) -> AuthResult<MinecraftAccount> {
        self.users.insert(account.id, account.clone());

        if self.default_user.is_none() {
            self.default_user = Some(account.id);
        }

        self.save().await?;
        tracing::debug!("committed account to credentials store");
        events
            .notify("Account added")
            .body(format!("Signed in as {}", account.username))
            .send();
        Ok(account)
    }

    pub fn add_offline_account(&mut self, username: String) -> AuthResult<MinecraftAccount> {
        let account = self.insert_offline_account(username)?;
        Ok(account)
    }

    #[tracing::instrument(level = "debug", skip(self), fields(username = %username))]
    pub async fn add_offline_account_and_save(
        &mut self,
        username: String,
    ) -> AuthResult<MinecraftAccount> {
        let account = self.insert_offline_account(username)?;
        self.save().await?;
        Ok(account)
    }

    fn insert_offline_account(&mut self, username: String) -> AuthResult<MinecraftAccount> {
        validate_offline_username(&username)?;

        if self
            .users
            .values()
            .any(|u| {
                u.kind == AccountKind::Offline && u.username.eq_ignore_ascii_case(&username)
            })
        {
            return Err(AuthError::DuplicateUsername { username });
        }

        let account = offline_account(username);
        self.users.insert(account.id, account.clone());
        Ok(account)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(%account.id))]
    pub async fn commit_refreshed_account(
        &mut self,
        account: MinecraftAccount,
    ) -> AuthResult<()> {
        self.users.insert(account.id, account);
        self.save().await?;
        tracing::debug!("stored refreshed Microsoft account");
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self), fields(%id))]
    pub async fn remove_account(&mut self, id: Uuid) -> AuthResult<Option<MinecraftAccount>> {
        let removed = self.users.remove(&id);

        if self.default_user == Some(id) {
            self.default_user = self
                .users
                .values()
                .find(|account| account.kind == AccountKind::Offline)
                .map(|account| account.id);
        }

        self.save().await?;
        Ok(removed)
    }

    #[tracing::instrument(level = "debug", skip(self), fields(?id))]
    pub async fn set_default_user(&mut self, id: Option<Uuid>) -> AuthResult<()> {
        if let Some(id) = id
            && self.get_account(id).is_none()
        {
            return Err(AuthError::AccountNotFound(id));
        }

        self.default_user = id;
        self.save().await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn resolve_default_id(&mut self) -> AuthResult<Option<Uuid>> {
        let id = self
            .default_user
            .filter(|id| self.get_account(*id).is_some())
            .or_else(|| {
                self.users
                    .values()
                    .find(|account| account.kind == AccountKind::Offline)
                    .map(|account| account.id)
            });

        let Some(id) = id else {
            return Ok(None);
        };

        if self.default_user != Some(id) {
            self.default_user = Some(id);
            self.save().await?;
        }

        Ok(Some(id))
    }

    /// Never touches the network callers that need a usable token go through
    /// [`crate::AuthService::account_for_launch`]
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn default_account(&mut self) -> AuthResult<Option<MinecraftAccount>> {
        let Some(id) = self.resolve_default_id().await? else {
            return Ok(None);
        };
        let Some(account) = self.users.get(&id).cloned() else {
            return Ok(None);
        };

        Ok(Some(account))
    }
}

/// A transient failure must keep the existing token discarding it because
/// Wi-Fi dropped would sign the user out of a working account
pub(crate) fn is_transient_auth_error(err: &AuthError) -> bool {
    matches!(
        err,
        AuthError::Minecraft(crate::error::MinecraftAuthError::RequestError { source, .. })
            if source.is_connect() || source.is_timeout()
    )
}
