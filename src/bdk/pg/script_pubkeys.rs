use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use tracing::instrument;
use uuid::Uuid;

use std::collections::HashMap;

use super::convert::BdkKeychainKind;
use crate::primitives::{bitcoin::ScriptBuf, *};

type ScriptWithPath = (ScriptBuf, (bdk::KeychainKind, u32));

pub struct ScriptPubkeys {
    keychain_id: KeychainId,
    pool: PgPool,
}

impl ScriptPubkeys {
    fn script_with_path(
        script: Vec<u8>,
        keychain_kind: BdkKeychainKind,
        path: i32,
    ) -> Result<ScriptWithPath, bdk::Error> {
        let path = u32::try_from(path)
            .map_err(|_| bdk::Error::Generic(format!("invalid derivation path from db: {path}")))?;
        Ok((
            ScriptBuf::from(script),
            (bdk::KeychainKind::from(keychain_kind), path),
        ))
    }

    pub fn new(keychain_id: KeychainId, pool: PgPool) -> Self {
        Self { keychain_id, pool }
    }

    #[instrument(name = "bdk.script_pubkeys.persist_all", skip_all)]
    // Retained for non-transactional call sites and focused tests.
    #[allow(dead_code)]
    pub async fn persist_all(
        &self,
        keys: Vec<(BdkKeychainKind, u32, ScriptBuf)>,
    ) -> Result<(), bdk::Error> {
        const BATCH_SIZE: usize = 5000;
        let chunks = keys.chunks(BATCH_SIZE);
        for chunk in chunks {
            let mut query_builder: QueryBuilder<Postgres> = QueryBuilder::new(
                r#"INSERT INTO bdk_script_pubkeys
        (keychain_id, keychain_kind, path, script, script_hex, script_fmt)"#,
            );

            query_builder.push_values(chunk, |mut builder, (keychain, path, script)| {
                builder.push_bind(self.keychain_id);
                builder.push_bind(keychain);
                builder.push_bind(*path as i32);
                builder.push_bind(script.as_bytes());
                builder.push_bind(format!("{script:02x}"));
                builder.push_bind(format!("{script:?}"));
            });
            query_builder.push("ON CONFLICT DO NOTHING");

            query_builder
                .build()
                .execute(&self.pool)
                .await
                .map_err(|e| bdk::Error::Generic(e.to_string()))?;
        }

        Ok(())
    }

    pub async fn persist_all_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        keys: Vec<(BdkKeychainKind, u32, ScriptBuf)>,
    ) -> Result<(), bdk::Error> {
        const BATCH_SIZE: usize = 5000;
        let chunks = keys.chunks(BATCH_SIZE);
        for chunk in chunks {
            let mut query_builder: QueryBuilder<Postgres> = QueryBuilder::new(
                r#"INSERT INTO bdk_script_pubkeys
        (keychain_id, keychain_kind, path, script, script_hex, script_fmt)"#,
            );

            query_builder.push_values(chunk, |mut builder, (keychain, path, script)| {
                builder.push_bind(self.keychain_id);
                builder.push_bind(keychain);
                builder.push_bind(*path as i32);
                builder.push_bind(script.as_bytes());
                builder.push_bind(format!("{script:02x}"));
                builder.push_bind(format!("{script:?}"));
            });
            query_builder.push("ON CONFLICT DO NOTHING");

            query_builder
                .build()
                .execute(tx.as_mut())
                .await
                .map_err(|e| bdk::Error::Generic(e.to_string()))?;
        }
        Ok(())
    }

    #[instrument(name = "bdk.script_pubkeys.find_script", skip_all)]
    pub async fn find_script(
        &self,
        keychain: impl Into<BdkKeychainKind>,
        path: u32,
    ) -> Result<Option<ScriptBuf>, bdk::Error> {
        let keychain_kind = keychain.into();
        let row = sqlx::query!(
            r#"SELECT script FROM bdk_script_pubkeys
            WHERE keychain_id = $1 AND keychain_kind = $2 AND path = $3"#,
            Uuid::from(self.keychain_id),
            keychain_kind as BdkKeychainKind,
            path as i32,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

        Ok(row.map(|row| ScriptBuf::from(row.script)))
    }

    #[instrument(name = "bdk.script_pubkeys.find_path", skip_all)]
    pub async fn find_path(
        &self,
        script: &ScriptBuf,
    ) -> Result<Option<(BdkKeychainKind, u32)>, bdk::Error> {
        let row = sqlx::query!(
            r#"SELECT keychain_kind as "keychain_kind: BdkKeychainKind", path FROM bdk_script_pubkeys
            WHERE keychain_id = $1 AND script_hex = ENCODE($2, 'hex')"#,
            Uuid::from(self.keychain_id),
            script.as_bytes(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

        row.map(|row| {
            u32::try_from(row.path)
                .map(|path| (row.keychain_kind, path))
                .map_err(|_| {
                    bdk::Error::Generic(format!("invalid derivation path from db: {}", row.path))
                })
        })
        .transpose()
    }

    #[instrument(name = "bdk.script_pubkeys.find_paths_for_scripts", skip_all, fields(n_requested = scripts.len(), n_found))]
    pub async fn find_paths_for_scripts(
        &self,
        scripts: &[ScriptBuf],
    ) -> Result<HashMap<ScriptBuf, (BdkKeychainKind, u32)>, bdk::Error> {
        if scripts.is_empty() {
            return Ok(HashMap::new());
        }

        let script_hexes: Vec<String> = scripts
            .iter()
            .map(|script| format!("{script:02x}"))
            .collect();
        let rows = sqlx::query(
            r#"SELECT script, keychain_kind, path
            FROM bdk_script_pubkeys
            WHERE keychain_id = $1 AND script_hex = ANY($2)"#,
        )
        .bind(Uuid::from(self.keychain_id))
        .bind(&script_hexes)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| bdk::Error::Generic(e.to_string()))?;

        tracing::Span::current().record("n_found", rows.len());

        rows.into_iter()
            .map(|row| {
                let keychain_kind_raw: BdkKeychainKind = row.get("keychain_kind");
                let path: i32 = row.get("path");
                let script: Vec<u8> = row.get("script");
                let (script, (_, path)) = Self::script_with_path(script, keychain_kind_raw, path)?;
                Ok((script, (keychain_kind_raw, path)))
            })
            .collect()
    }

    #[instrument(name = "bdk.script_pubkeys.list_scripts", skip_all)]
    // Retained for non-path call sites and focused tests.
    #[allow(dead_code)]
    pub async fn list_scripts(
        &self,
        keychain: Option<impl Into<BdkKeychainKind>>,
    ) -> Result<Vec<ScriptBuf>, bdk::Error> {
        let keychain_id = Uuid::from(self.keychain_id);
        let keychain_kind: Option<BdkKeychainKind> = keychain.map(Into::into);
        let scripts = if let Some(keychain_kind) = keychain_kind {
            sqlx::query_scalar!(
                r#"SELECT script FROM bdk_script_pubkeys
                WHERE keychain_id = $1 AND keychain_kind = $2"#,
                keychain_id,
                keychain_kind as BdkKeychainKind,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| bdk::Error::Generic(e.to_string()))?
        } else {
            sqlx::query_scalar!(
                r#"SELECT script FROM bdk_script_pubkeys
                WHERE keychain_id = $1"#,
                keychain_id,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| bdk::Error::Generic(e.to_string()))?
        };

        Ok(scripts.into_iter().map(ScriptBuf::from).collect())
    }

    #[instrument(name = "bdk.script_pubkeys.list_scripts_with_paths", skip_all)]
    pub async fn list_scripts_with_paths(
        &self,
        keychain: Option<impl Into<BdkKeychainKind>>,
    ) -> Result<Vec<ScriptWithPath>, bdk::Error> {
        let keychain_id = Uuid::from(self.keychain_id);
        let keychain_kind: Option<BdkKeychainKind> = keychain.map(Into::into);
        if let Some(keychain_kind) = keychain_kind {
            let rows = sqlx::query!(
                r#"SELECT script, keychain_kind as "keychain_kind: BdkKeychainKind", path FROM bdk_script_pubkeys
                WHERE keychain_id = $1 AND keychain_kind = $2"#,
                keychain_id,
                keychain_kind as BdkKeychainKind,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| bdk::Error::Generic(e.to_string()))?;

            Ok(rows
                .into_iter()
                .map(|row| Self::script_with_path(row.script, row.keychain_kind, row.path))
                .collect::<Result<Vec<_>, _>>()?)
        } else {
            let rows = sqlx::query!(
                r#"SELECT script, keychain_kind as "keychain_kind: BdkKeychainKind", path FROM bdk_script_pubkeys
                WHERE keychain_id = $1"#,
                keychain_id,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| bdk::Error::Generic(e.to_string()))?;

            Ok(rows
                .into_iter()
                .map(|row| Self::script_with_path(row.script, row.keychain_kind, row.path))
                .collect::<Result<Vec<_>, _>>()?)
        }
    }
}
