use database_entity::dto::{
  AFRole, AFWorkspaceInvitation, AFWorkspaceInvitationStatus, AFWorkspaceSettings,
  PublishCollabItem, PublishInfo,
};
use futures_util::stream::BoxStream;
use sqlx::{types::uuid, Executor, PgPool, Postgres, Transaction};
use std::{collections::HashMap, ops::DerefMut};
use tracing::{event, instrument};
use uuid::Uuid;

use crate::pg_row::AFWorkspaceMemberPermRow;
use crate::pg_row::{
  AFPermissionRow, AFUserProfileRow, AFWorkspaceInvitationMinimal, AFWorkspaceMemberRow,
  AFWorkspaceRow,
};
use crate::user::select_uid_from_email;
use app_error::AppError;

#[inline]
pub async fn delete_from_workspace(pg_pool: &PgPool, workspace_id: &Uuid) -> Result<(), AppError> {
  let pg_row = sqlx::query!(
    r#"
    DELETE FROM public.af_workspace where workspace_id = $1
    "#,
    workspace_id
  )
  .execute(pg_pool)
  .await?;

  assert!(pg_row.rows_affected() == 1);
  Ok(())
}

#[inline]
pub async fn insert_user_workspace(
  tx: &mut Transaction<'_, sqlx::Postgres>,
  user_uuid: &Uuid,
  workspace_name: &str,
) -> Result<AFWorkspaceRow, AppError> {
  let workspace = sqlx::query_as!(
    AFWorkspaceRow,
    r#"
    INSERT INTO public.af_workspace (owner_uid, workspace_name)
    VALUES ((SELECT uid FROM public.af_user WHERE uuid = $1), $2)
    RETURNING
      workspace_id,
      database_storage_id,
      owner_uid,
      (SELECT name FROM public.af_user WHERE uid = owner_uid) AS owner_name,
      created_at,
      workspace_type,
      deleted_at,
      workspace_name,
      icon
    ;
    "#,
    user_uuid,
    workspace_name,
  )
  .fetch_one(tx.deref_mut())
  .await?;

  Ok(workspace)
}

#[inline]
pub async fn rename_workspace(
  tx: &mut Transaction<'_, sqlx::Postgres>,
  workspace_id: &Uuid,
  new_workspace_name: &str,
) -> Result<(), AppError> {
  let res = sqlx::query!(
    r#"
      UPDATE public.af_workspace
      SET workspace_name = $1
      WHERE workspace_id = $2
    "#,
    new_workspace_name,
    workspace_id,
  )
  .execute(tx.deref_mut())
  .await?;

  if res.rows_affected() != 1 {
    tracing::error!("Failed to rename workspace, workspace_id: {}", workspace_id);
  }
  Ok(())
}

#[inline]
pub async fn change_workspace_icon(
  tx: &mut Transaction<'_, sqlx::Postgres>,
  workspace_id: &Uuid,
  icon: &str,
) -> Result<(), AppError> {
  let res = sqlx::query!(
    r#"
      UPDATE public.af_workspace
      SET icon = $1
      WHERE workspace_id = $2
    "#,
    icon,
    workspace_id,
  )
  .execute(tx.deref_mut())
  .await?;

  if res.rows_affected() != 1 {
    tracing::error!(
      "Failed to change workspace icon, workspace_id: {}",
      workspace_id
    );
  }
  Ok(())
}

/// Checks whether a user, identified by a UUID, is an 'Owner' of a workspace, identified by its
/// workspace_id.
#[inline]
pub async fn select_user_is_workspace_owner(
  pg_pool: &PgPool,
  user_uuid: &Uuid,
  workspace_uuid: &Uuid,
) -> Result<bool, AppError> {
  let exists = sqlx::query_scalar!(
    r#"
  SELECT EXISTS(
    SELECT 1
    FROM public.af_workspace_member
      JOIN af_roles ON af_workspace_member.role_id = af_roles.id
    WHERE workspace_id = $1
    AND af_workspace_member.uid = (
      SELECT uid FROM public.af_user WHERE uuid = $2
    )
    AND af_roles.name = 'Owner'
  ) AS "exists";
  "#,
    workspace_uuid,
    user_uuid
  )
  .fetch_one(pg_pool)
  .await?;

  Ok(exists.unwrap_or(false))
}

pub async fn select_user_is_collab_publisher_for_all_views(
  pg_pool: &PgPool,
  user_uuid: &Uuid,
  workspace_uuid: &Uuid,
  view_ids: &[Uuid],
) -> Result<bool, AppError> {
  let count = sqlx::query_scalar!(
    r#"
      SELECT COUNT(*)
      FROM af_published_collab
      WHERE workspace_id = $1
        AND view_id = ANY($2)
        AND published_by = (SELECT uid FROM af_user WHERE uuid = $3)
    "#,
    workspace_uuid,
    view_ids,
    user_uuid,
  )
  .fetch_one(pg_pool)
  .await?;

  match count {
    Some(c) => Ok(c == view_ids.len() as i64),
    None => Ok(false),
  }
}

#[inline]
pub async fn select_user_role<'a, E: Executor<'a, Database = Postgres>>(
  exectuor: E,
  uid: &i64,
  workspace_uuid: &Uuid,
) -> Result<AFRole, AppError> {
  let row = sqlx::query_scalar!(
    r#"
     SELECT role_id FROM af_workspace_member
     WHERE workspace_id = $1 AND uid = $2
    "#,
    workspace_uuid,
    uid
  )
  .fetch_one(exectuor)
  .await?;
  Ok(AFRole::from(row))
}

/// Checks the user's permission to edit a collab object.
/// user can edit collab if:
/// 1. user is the member of the workspace
/// 2. the collab object is not exist
/// 3. the collab object is exist and the user is the member of the collab and the role is owner or member
#[allow(dead_code)]
pub async fn select_user_can_edit_collab(
  pg_pool: &PgPool,
  user_uuid: &Uuid,
  workspace_id: &Uuid,
  object_id: &str,
) -> Result<bool, AppError> {
  let permission_check = sqlx::query_scalar!(
    r#"
    WITH workspace_check AS (
        SELECT EXISTS(
            SELECT 1
            FROM af_workspace_member
            WHERE af_workspace_member.uid = (SELECT uid FROM af_user WHERE uuid = $1) AND
            af_workspace_member.workspace_id = $3
        ) AS "workspace_exists"
    ),
    collab_check AS (
        SELECT EXISTS(
            SELECT 1
            FROM af_collab_member
            WHERE oid = $2
        ) AS "collab_exists"
    )
    SELECT
        NOT collab_check.collab_exists OR (
            workspace_check.workspace_exists AND
            EXISTS(
                SELECT 1
                FROM af_collab_member
                JOIN af_permissions ON af_collab_member.permission_id = af_permissions.id
                WHERE
                    af_collab_member.uid = (SELECT uid FROM af_user WHERE uuid = $1) AND
                    af_collab_member.oid = $2 AND
                    af_permissions.access_level > 20
            )
        ) AS "permission_check"
    FROM workspace_check, collab_check;
     "#,
    user_uuid,
    object_id,
    workspace_id,
  )
  .fetch_one(pg_pool)
  .await?;

  Ok(permission_check.unwrap_or(false))
}

#[inline]
pub async fn upsert_workspace_member_with_txn(
  txn: &mut Transaction<'_, sqlx::Postgres>,
  workspace_id: &uuid::Uuid,
  member_email: &str,
  role: AFRole,
) -> Result<(), AppError> {
  let role_id: i32 = role.into();
  sqlx::query!(
    r#"
      INSERT INTO public.af_workspace_member (workspace_id, uid, role_id)
      SELECT $1, af_user.uid, $3
      FROM public.af_user
      WHERE
        af_user.email = $2
      ON CONFLICT (workspace_id, uid)
      DO NOTHING;
    "#,
    workspace_id,
    member_email,
    role_id
  )
  .execute(txn.deref_mut())
  .await?;

  Ok(())
}

#[inline]
pub async fn insert_workspace_invitation(
  txn: &mut Transaction<'_, sqlx::Postgres>,
  invite_id: &uuid::Uuid,
  workspace_id: &uuid::Uuid,
  inviter_uuid: &Uuid,
  invitee_email: &str,
  invitee_role: AFRole,
) -> Result<(), AppError> {
  let role_id: i32 = invitee_role.into();
  sqlx::query!(
    r#"
      INSERT INTO public.af_workspace_invitation (
          id,
          workspace_id,
          inviter,
          invitee_email,
          role_id
      )
      VALUES (
        $1,
        $2,
        (SELECT uid FROM public.af_user WHERE uuid = $3),
        $4,
        $5
      )
    "#,
    invite_id,
    workspace_id,
    inviter_uuid,
    invitee_email,
    role_id
  )
  .execute(txn.deref_mut())
  .await?;

  Ok(())
}

pub async fn update_workspace_invitation_set_status_accepted(
  txn: &mut Transaction<'_, sqlx::Postgres>,
  invitee_uuid: &Uuid,
  invite_id: &Uuid,
) -> Result<(), AppError> {
  let res = sqlx::query_scalar!(
    r#"
    UPDATE public.af_workspace_invitation
    SET status = 1
    WHERE invitee_email = (SELECT email FROM public.af_user WHERE uuid = $1)
      AND id = $2
      AND status = 0
    "#,
    invitee_uuid,
    invite_id,
  )
  .execute(txn.deref_mut())
  .await?;
  match res.rows_affected() {
    0 => Err(AppError::RecordNotFound(format!(
      "Invitation not found, invitee_uuid: {}, invite_id: {}",
      invitee_uuid, invite_id
    ))),
    1 => Ok(()),
    x => Err(
      anyhow::anyhow!(
        "Expected 1 row to be affected, but {} rows were affected",
        x
      )
      .into(),
    ),
  }
}

pub async fn get_invitation_by_id(
  txn: &mut Transaction<'_, sqlx::Postgres>,
  invite_id: &Uuid,
) -> Result<AFWorkspaceInvitationMinimal, AppError> {
  let res = sqlx::query_as!(
    AFWorkspaceInvitationMinimal,
    r#"
    SELECT
        workspace_id,
        inviter AS inviter_uid,
        (SELECT uid FROM public.af_user WHERE email = invitee_email) AS invitee_uid,
        status,
        role_id AS role
    FROM
    public.af_workspace_invitation
    WHERE id = $1
    "#,
    invite_id,
  )
  .fetch_one(txn.deref_mut())
  .await?;

  Ok(res)
}

#[inline]
pub async fn select_workspace_invitations_for_user(
  pg_pool: &PgPool,
  invitee_uuid: &Uuid,
  status_filter: Option<AFWorkspaceInvitationStatus>,
) -> Result<Vec<AFWorkspaceInvitation>, AppError> {
  let res = sqlx::query_as!(
    AFWorkspaceInvitation,
    r#"
    SELECT
      id AS invite_id,
      workspace_id,
      (SELECT workspace_name FROM public.af_workspace WHERE workspace_id = af_workspace_invitation.workspace_id),
      (SELECT email FROM public.af_user WHERE uid = af_workspace_invitation.inviter) AS inviter_email,
      (SELECT name FROM public.af_user WHERE uid = af_workspace_invitation.inviter) AS inviter_name,
      status,
      updated_at
    FROM
      public.af_workspace_invitation
    WHERE af_workspace_invitation.invitee_email = (SELECT email FROM public.af_user WHERE uuid = $1)
    AND ($2::SMALLINT IS NULL OR status = $2)
    "#,
    invitee_uuid,
    status_filter.map(|s| s as i16)
  ).fetch_all(pg_pool).await?;
  Ok(res)
}

#[inline]
#[instrument(level = "trace", skip(pool, email, role), err)]
pub async fn upsert_workspace_member(
  pool: &PgPool,
  workspace_id: &Uuid,
  email: &str,
  role: AFRole,
) -> Result<(), sqlx::Error> {
  event!(
    tracing::Level::TRACE,
    "update workspace member: workspace_id:{}, uid {:?}, role:{:?}",
    workspace_id,
    select_uid_from_email(pool, email).await,
    role
  );

  let role_id: i32 = role.into();
  sqlx::query!(
    r#"
        UPDATE af_workspace_member
        SET
            role_id = $1
        WHERE workspace_id = $2 AND uid = (
            SELECT uid FROM af_user WHERE email = $3
        )
        "#,
    role_id,
    workspace_id,
    email
  )
  .execute(pool)
  .await?;

  Ok(())
}

#[inline]
pub async fn delete_workspace_members(
  txn: &mut Transaction<'_, sqlx::Postgres>,
  workspace_id: &Uuid,
  member_email: &str,
) -> Result<(), AppError> {
  let is_owner = sqlx::query_scalar!(
    r#"
  SELECT EXISTS (
    SELECT 1
    FROM public.af_workspace
    WHERE
        workspace_id = $1
        AND owner_uid = (
            SELECT uid FROM public.af_user WHERE email = $2
        )
   ) AS "is_owner";
  "#,
    workspace_id,
    member_email
  )
  .fetch_one(txn.deref_mut())
  .await?
  .unwrap_or(false);

  if is_owner {
    return Err(AppError::NotEnoughPermissions {
      user: member_email.to_string(),
      action: format!("delete member from workspace {}", workspace_id),
    });
  }

  sqlx::query!(
    r#"
    DELETE FROM public.af_workspace_member
    WHERE
    workspace_id = $1
    AND uid = (
        SELECT uid FROM public.af_user WHERE email = $2
    )
    -- Ensure the user to be deleted is not the original owner.
    -- 1. TODO(nathan): User must transfer ownership to another user first.
    -- 2. User must have at least one workspace
    AND uid <> (
        SELECT owner_uid FROM public.af_workspace WHERE workspace_id = $1
    );
    "#,
    workspace_id,
    member_email,
  )
  .execute(txn.deref_mut())
  .await?;

  Ok(())
}

pub fn select_workspace_member_perm_stream(
  pg_pool: &PgPool,
) -> BoxStream<'_, sqlx::Result<AFWorkspaceMemberPermRow>> {
  sqlx::query_as!(
    AFWorkspaceMemberPermRow,
    "SELECT uid, role_id as role, workspace_id FROM af_workspace_member"
  )
  .fetch(pg_pool)
}

/// returns a list of workspace members, sorted by their creation time.
#[inline]
pub async fn select_workspace_member_list(
  pg_pool: &PgPool,
  workspace_id: &uuid::Uuid,
) -> Result<Vec<AFWorkspaceMemberRow>, AppError> {
  let members = sqlx::query_as!(
    AFWorkspaceMemberRow,
    r#"
    SELECT af_user.uid, af_user.name, af_user.email,
    af_workspace_member.role_id AS role
    FROM public.af_workspace_member
        JOIN public.af_user ON af_workspace_member.uid = af_user.uid
    WHERE af_workspace_member.workspace_id = $1
    ORDER BY af_workspace_member.created_at ASC;
    "#,
    workspace_id
  )
  .fetch_all(pg_pool)
  .await?;
  Ok(members)
}

#[inline]
pub async fn select_workspace_member<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  uid: &i64,
  workspace_id: &Uuid,
) -> Result<AFWorkspaceMemberRow, AppError> {
  let member = sqlx::query_as!(
    AFWorkspaceMemberRow,
    r#"
    SELECT af_user.uid, af_user.name, af_user.email, af_workspace_member.role_id AS role
    FROM public.af_workspace_member
      JOIN public.af_user ON af_workspace_member.uid = af_user.uid
    WHERE af_workspace_member.workspace_id = $1
    AND af_workspace_member.uid = $2
    "#,
    workspace_id,
    uid,
  )
  .fetch_one(executor)
  .await?;
  Ok(member)
}

#[inline]
pub async fn select_user_profile<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  user_uuid: &Uuid,
) -> Result<Option<AFUserProfileRow>, AppError> {
  let user_profile = sqlx::query_as!(
    AFUserProfileRow,
    r#"
      SELECT *
      FROM public.af_user_profile_view WHERE uuid = $1
    "#,
    user_uuid
  )
  .fetch_optional(executor)
  .await?;
  Ok(user_profile)
}

#[inline]
pub async fn select_workspace<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
) -> Result<AFWorkspaceRow, AppError> {
  let workspace = sqlx::query_as!(
    AFWorkspaceRow,
    r#"
      SELECT
        workspace_id,
        database_storage_id,
        owner_uid,
        (SELECT name FROM public.af_user WHERE uid = owner_uid) AS owner_name,
        created_at,
        workspace_type,
        deleted_at,
        workspace_name,
        icon
      FROM public.af_workspace WHERE workspace_id = $1
    "#,
    workspace_id
  )
  .fetch_one(executor)
  .await?;
  Ok(workspace)
}
#[inline]
pub async fn update_updated_at_of_workspace<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  user_uuid: &Uuid,
  workspace_id: &Uuid,
) -> Result<(), AppError> {
  sqlx::query!(
    r#"
       UPDATE af_workspace_member
       SET updated_at = CURRENT_TIMESTAMP
       WHERE uid = (SELECT uid FROM public.af_user WHERE uuid = $1) AND workspace_id = $2;
    "#,
    user_uuid,
    workspace_id
  )
  .execute(executor)
  .await?;
  Ok(())
}

/// Returns a list of workspaces that the user is part of.
/// User may owner or non-owner.
#[inline]
pub async fn select_all_user_workspaces<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  user_uuid: &Uuid,
) -> Result<Vec<AFWorkspaceRow>, AppError> {
  let workspaces = sqlx::query_as!(
    AFWorkspaceRow,
    r#"
      SELECT
        w.workspace_id,
        w.database_storage_id,
        w.owner_uid,
        (SELECT name FROM public.af_user WHERE uid = w.owner_uid) AS owner_name,
        w.created_at,
        w.workspace_type,
        w.deleted_at,
        w.workspace_name,
        w.icon
      FROM af_workspace w
      JOIN af_workspace_member wm ON w.workspace_id = wm.workspace_id
      WHERE wm.uid = (
         SELECT uid FROM public.af_user WHERE uuid = $1
      );
    "#,
    user_uuid
  )
  .fetch_all(executor)
  .await?;
  Ok(workspaces)
}

pub async fn select_permission(
  pool: &PgPool,
  permission_id: &i64,
) -> Result<Option<AFPermissionRow>, AppError> {
  let permission = sqlx::query_as!(
    AFPermissionRow,
    r#"
      SELECT * FROM public.af_permissions WHERE id = $1
    "#,
    *permission_id as i32
  )
  .fetch_optional(pool)
  .await?;
  Ok(permission)
}

pub async fn select_permission_from_role_id(
  pool: &PgPool,
  role_id: &i64,
) -> Result<Option<AFPermissionRow>, AppError> {
  let permission = sqlx::query_as!(
    AFPermissionRow,
    r#"
    SELECT p.id, p.name, p.access_level, p.description FROM af_permissions p
    JOIN af_role_permissions rp ON p.id = rp.permission_id
    WHERE rp.role_id = $1
    "#,
    *role_id as i32
  )
  .fetch_optional(pool)
  .await?;
  Ok(permission)
}

pub async fn select_workspace_total_collab_bytes(
  pool: &PgPool,
  workspace_id: &Uuid,
) -> Result<i64, AppError> {
  let sum = sqlx::query_scalar!(
    r#"
    SELECT SUM(len) FROM af_collab WHERE workspace_id = $1
    "#,
    workspace_id
  )
  .fetch_one(pool)
  .await?;

  match sum {
    Some(s) => Ok(s),
    None => Err(AppError::RecordNotFound(format!(
      "Failed to get total collab bytes for workspace_id: {}",
      workspace_id
    ))),
  }
}

#[inline]
pub async fn select_workspace_name_from_workspace_id(
  pool: &PgPool,
  workspace_id: &Uuid,
) -> Result<Option<String>, AppError> {
  let workspace_name = sqlx::query_scalar!(
    r#"
      SELECT workspace_name
      FROM public.af_workspace
      WHERE workspace_id = $1
    "#,
    workspace_id
  )
  .fetch_one(pool)
  .await?;
  Ok(workspace_name)
}

#[inline]
pub async fn select_workspace_member_count_from_workspace_id(
  pool: &PgPool,
  workspace_id: &Uuid,
) -> Result<Option<i64>, AppError> {
  let workspace_count = sqlx::query_scalar!(
    r#"
      SELECT COUNT(*)
      FROM public.af_workspace_member
      WHERE workspace_id = $1
    "#,
    workspace_id
  )
  .fetch_one(pool)
  .await?;
  Ok(workspace_count)
}

#[inline]
pub async fn select_workspace_pending_invitations(
  pool: &PgPool,
  workspace_id: &Uuid,
) -> Result<HashMap<String, Uuid>, AppError> {
  let res = sqlx::query!(
    r#"
      SELECT id, invitee_email
      FROM public.af_workspace_invitation
      WHERE workspace_id = $1
      AND status = 0
    "#,
    workspace_id
  )
  .fetch_all(pool)
  .await?;

  let inv_id_by_email = res
    .into_iter()
    .map(|row| (row.invitee_email, row.id))
    .collect::<HashMap<String, Uuid>>();

  Ok(inv_id_by_email)
}

#[inline]
pub async fn is_workspace_exist<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
) -> Result<bool, AppError> {
  let exists = sqlx::query_scalar!(
    r#"
    SELECT EXISTS(
      SELECT 1
      FROM af_workspace
      WHERE workspace_id = $1
    ) AS user_exists;
  "#,
    workspace_id
  )
  .fetch_one(executor)
  .await?;

  Ok(exists.unwrap_or(false))
}

pub async fn select_workspace_settings<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
) -> Result<Option<AFWorkspaceSettings>, AppError> {
  let json = sqlx::query_scalar!(
    r#"SElECT settings FROM af_workspace WHERE workspace_id = $1"#,
    workspace_id
  )
  .fetch_one(executor)
  .await?;

  match json {
    None => Ok(None),
    Some(value) => {
      let settings: AFWorkspaceSettings = serde_json::from_value(value)?;
      Ok(Some(settings))
    },
  }
}
pub async fn upsert_workspace_settings(
  tx: &mut Transaction<'_, Postgres>,
  workspace_id: &Uuid,
  settings: &AFWorkspaceSettings,
) -> Result<(), AppError> {
  let json = serde_json::to_value(settings)?;
  sqlx::query!(
    r#"UPDATE af_workspace SET settings = $1 WHERE workspace_id = $2"#,
    json,
    workspace_id
  )
  .execute(tx.deref_mut())
  .await?;

  Ok(())
}

#[inline]
pub async fn select_workspace_publish_namespace_exists<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
  namespace: &str,
) -> Result<bool, AppError> {
  let res = sqlx::query_scalar!(
    r#"
      SELECT EXISTS(
        SELECT 1
        FROM af_workspace
        WHERE workspace_id = $1
          AND publish_namespace = $2
      )
    "#,
    workspace_id,
    namespace,
  )
  .fetch_one(executor)
  .await?;

  Ok(res.unwrap_or(false))
}

#[inline]
pub async fn update_workspace_publish_namespace<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
  new_namespace: &str,
) -> Result<(), AppError> {
  let res = sqlx::query!(
    r#"
      UPDATE af_workspace
      SET publish_namespace = $1
      WHERE workspace_id = $2
    "#,
    new_namespace,
    workspace_id,
  )
  .execute(executor)
  .await?;

  if res.rows_affected() != 1 {
    tracing::error!(
      "Failed to update workspace publish namespace, workspace_id: {}, new_namespace: {}, rows_affected: {}",
      workspace_id, new_namespace, res.rows_affected()
    );
  }

  Ok(())
}

#[inline]
pub async fn select_workspace_publish_namespace<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
) -> Result<Option<String>, AppError> {
  let res = sqlx::query_scalar!(
    r#"
      SELECT publish_namespace
      FROM af_workspace
      WHERE workspace_id = $1
    "#,
    workspace_id,
  )
  .fetch_one(executor)
  .await?;

  Ok(res)
}

#[inline]
pub async fn insert_or_replace_publish_collab_metas<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
  publisher_uuid: &Uuid,
  publish_item: &[PublishCollabItem<serde_json::Value, Vec<u8>>],
) -> Result<(), AppError> {
  let view_ids: Vec<Uuid> = publish_item.iter().map(|item| item.meta.view_id).collect();
  let publish_names: Vec<String> = publish_item
    .iter()
    .map(|item| item.meta.publish_name.clone())
    .collect();
  let metadatas: Vec<serde_json::Value> = publish_item
    .iter()
    .map(|item| item.meta.metadata.clone())
    .collect();

  let blobs: Vec<Vec<u8>> = publish_item.iter().map(|item| item.data.clone()).collect();
  let res = sqlx::query!(
    r#"
      INSERT INTO af_published_collab (workspace_id, view_id, publish_name, published_by, metadata, blob)
      SELECT * FROM UNNEST(
        (SELECT array_agg((SELECT $1::uuid)) FROM generate_series(1, $7))::uuid[],
        $2::uuid[],
        $3::text[],
        (SELECT array_agg((SELECT uid FROM af_user WHERE uuid = $4)) FROM generate_series(1, $7))::bigint[],
        $5::jsonb[],
        $6::bytea[]
      )
      ON CONFLICT (workspace_id, view_id) DO UPDATE
      SET metadata = EXCLUDED.metadata
    "#,
    workspace_id,
    &view_ids,
    &publish_names,
    publisher_uuid,
    &metadatas,
    &blobs,
    publish_item.len() as i32,
  )
  .execute(executor)
  .await?;

  if res.rows_affected() != publish_item.len() as u64 {
    tracing::warn!(
      "Failed to insert or replace publish collab meta batch, workspace_id: {}, publisher_uuid: {}, rows_affected: {}",
      workspace_id, publisher_uuid, res.rows_affected()
    );
  }

  Ok(())
}

#[inline]
pub async fn select_publish_collab_meta<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  publish_namespace: &str,
  publish_name: &str,
) -> Result<serde_json::Value, AppError> {
  let res = sqlx::query!(
    r#"
    SELECT metadata
    FROM af_published_collab
    WHERE workspace_id = (SELECT workspace_id FROM af_workspace WHERE publish_namespace = $1)
      AND publish_name = $2
    "#,
    publish_namespace,
    publish_name,
  )
  .fetch_one(executor)
  .await?;
  let metadata: serde_json::Value = res.metadata;
  Ok(metadata)
}

#[inline]
pub async fn delete_published_collabs<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  workspace_id: &Uuid,
  view_ids: &[Uuid],
) -> Result<(), AppError> {
  let res = sqlx::query!(
    r#"
      DELETE FROM af_published_collab
      WHERE workspace_id = $1
        AND view_id = ANY($2)
    "#,
    workspace_id,
    view_ids,
  )
  .execute(executor)
  .await?;

  if res.rows_affected() != view_ids.len() as u64 {
    tracing::error!(
      "Failed to delete published collabs, workspace_id: {}, view_ids: {:?}, rows_affected: {}",
      workspace_id,
      view_ids,
      res.rows_affected()
    );
  }

  Ok(())
}

#[inline]
pub async fn select_published_collab_blob<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  publish_namespace: &str,
  publish_name: &str,
) -> Result<Vec<u8>, AppError> {
  let res = sqlx::query_scalar!(
    r#"
      SELECT blob
      FROM af_published_collab
      WHERE workspace_id = (SELECT workspace_id FROM af_workspace WHERE publish_namespace = $1)
      AND publish_name = $2
    "#,
    publish_namespace,
    publish_name,
  )
  .fetch_one(executor)
  .await?;

  Ok(res)
}

pub async fn select_published_collab_info<'a, E: Executor<'a, Database = Postgres>>(
  executor: E,
  view_id: &Uuid,
) -> Result<PublishInfo, AppError> {
  let res = sqlx::query_as!(
    PublishInfo,
    r#"
      SELECT
        (SELECT publish_namespace FROM af_workspace aw WHERE aw.workspace_id = apc.workspace_id) AS namespace,
        publish_name,
        view_id
      FROM af_published_collab apc
      WHERE view_id = $1
    "#,
    view_id,
  )
  .fetch_one(executor)
  .await?;

  Ok(res)
}
