use std::collections::{HashMap, HashSet};

use std::ops::Deref;
use std::sync::{Arc, Weak};

use appflowy_integrate::collab_builder::AppFlowyCollabBuilder;
use collab::core::collab_state::CollabState;

use collab_folder::core::{
  Folder, FolderContext, TrashChange, TrashChangeReceiver, TrashInfo, TrashRecord, View,
  ViewChange, ViewChangeReceiver, ViewLayout, Workspace,
};
use parking_lot::Mutex;
use tracing::{event, Level};

use crate::deps::{FolderCloudService, FolderUser};
use flowy_error::{ErrorCode, FlowyError, FlowyResult};
use lib_infra::util::timestamp;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

use crate::entities::{
  view_pb_with_child_views, CreateViewParams, CreateWorkspaceParams, RepeatedTrashPB,
  RepeatedViewPB, RepeatedWorkspacePB, UpdateViewParams, ViewPB,
};
use crate::notification::{
  send_notification, send_workspace_notification, send_workspace_setting_notification,
  FolderNotification,
};
use crate::share::ImportParams;
use crate::user_default::DefaultFolderBuilder;
use crate::view_operation::{
  create_view, gen_view_id, FolderOperationHandler, FolderOperationHandlers,
};

pub struct Folder2Manager {
  mutex_folder: Arc<MutexFolder>,
  collab_builder: Arc<AppFlowyCollabBuilder>,
  user: Arc<dyn FolderUser>,
  operation_handlers: FolderOperationHandlers,
  cloud_service: Arc<dyn FolderCloudService>,
}

unsafe impl Send for Folder2Manager {}
unsafe impl Sync for Folder2Manager {}

impl Folder2Manager {
  pub async fn new(
    user: Arc<dyn FolderUser>,
    collab_builder: Arc<AppFlowyCollabBuilder>,
    operation_handlers: FolderOperationHandlers,
    cloud_service: Arc<dyn FolderCloudService>,
  ) -> FlowyResult<Self> {
    let mutex_folder = Arc::new(MutexFolder::default());
    let manager = Self {
      user,
      mutex_folder,
      collab_builder,
      operation_handlers,
      cloud_service,
    };

    Ok(manager)
  }

  pub async fn get_current_workspace(&self) -> FlowyResult<Workspace> {
    match self.with_folder(None, |folder| folder.get_current_workspace()) {
      None => Err(FlowyError::record_not_found().context("Can not find the workspace")),
      Some(workspace) => Ok(workspace),
    }
  }

  pub async fn get_current_workspace_views(&self) -> FlowyResult<Vec<ViewPB>> {
    let workspace_id = self
      .mutex_folder
      .lock()
      .as_ref()
      .map(|folder| folder.get_current_workspace_id());

    if let Some(Some(workspace_id)) = workspace_id {
      self.get_workspace_views(&workspace_id).await
    } else {
      Ok(vec![])
    }
  }

  pub async fn get_workspace_views(&self, workspace_id: &str) -> FlowyResult<Vec<ViewPB>> {
    let views = self.with_folder(vec![], |folder| {
      get_workspace_view_pbs(workspace_id, folder)
    });

    Ok(views)
  }

  /// Called immediately after the application launched fi the user already sign in/sign up.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn initialize(&self, uid: i64, workspace_id: &str) -> FlowyResult<()> {
    let workspace_id = workspace_id.to_string();
    if let Ok(collab_db) = self.user.collab_db() {
      let collab = self
        .collab_builder
        .build(uid, &workspace_id, "workspace", collab_db);
      let (view_tx, view_rx) = tokio::sync::broadcast::channel(100);
      let (trash_tx, trash_rx) = tokio::sync::broadcast::channel(100);
      let folder_context = FolderContext {
        view_change_tx: view_tx,
        trash_change_tx: trash_tx,
      };
      let folder = Folder::get_or_create(collab, folder_context);
      let folder_state_rx = folder.subscribe_state_change();
      *self.mutex_folder.lock() = Some(folder);

      let weak_mutex_folder = Arc::downgrade(&self.mutex_folder);
      listen_on_folder_state_change(workspace_id, folder_state_rx, &weak_mutex_folder);
      listen_on_trash_change(trash_rx, &weak_mutex_folder);
      listen_on_view_change(view_rx, &weak_mutex_folder);
    }

    Ok(())
  }

  /// Called after the user sign up / sign in
  pub async fn initialize_with_new_user(
    &self,
    user_id: i64,
    token: &str,
    workspace_id: &str,
  ) -> FlowyResult<()> {
    self.initialize(user_id, workspace_id).await?;
    let (folder_data, workspace_pb) = DefaultFolderBuilder::build(
      self.user.user_id()?,
      workspace_id.to_string(),
      &self.operation_handlers,
    )
    .await;
    self.with_folder((), |folder| {
      folder.create_with_data(folder_data);
    });

    send_notification(token, FolderNotification::DidCreateWorkspace)
      .payload(RepeatedWorkspacePB {
        items: vec![workspace_pb],
      })
      .send();
    Ok(())
  }

  /// Called when the current user logout
  ///
  pub async fn clear(&self, _user_id: i64) {}

  pub async fn create_workspace(&self, params: CreateWorkspaceParams) -> FlowyResult<Workspace> {
    let workspace = self
      .cloud_service
      .create_workspace(self.user.user_id()?, &params.name)
      .await?;
    self.with_folder((), |folder| {
      folder.workspaces.create_workspace(workspace.clone());
      folder.set_current_workspace(&workspace.id);
    });

    let repeated_workspace = RepeatedWorkspacePB {
      items: vec![workspace.clone().into()],
    };
    send_workspace_notification(FolderNotification::DidCreateWorkspace, repeated_workspace);
    Ok(workspace)
  }

  pub async fn open_workspace(&self, workspace_id: &str) -> FlowyResult<Workspace> {
    self.with_folder(Err(FlowyError::internal()), |folder| {
      let workspace = folder
        .workspaces
        .get_workspace(workspace_id)
        .ok_or_else(|| {
          FlowyError::record_not_found().context("Can't open not existing workspace")
        })?;
      folder.set_current_workspace(workspace_id);
      Ok::<Workspace, FlowyError>(workspace)
    })
  }

  pub async fn get_workspace(&self, workspace_id: &str) -> Option<Workspace> {
    self.with_folder(None, |folder| folder.workspaces.get_workspace(workspace_id))
  }

  fn with_folder<F, Output>(&self, default_value: Output, f: F) -> Output
  where
    F: FnOnce(&Folder) -> Output,
  {
    let folder = self.mutex_folder.lock();
    match &*folder {
      None => default_value,
      Some(folder) => f(folder),
    }
  }

  pub async fn get_all_workspaces(&self) -> Vec<Workspace> {
    self.with_folder(vec![], |folder| folder.workspaces.get_all_workspaces())
  }

  pub async fn create_view_with_params(&self, params: CreateViewParams) -> FlowyResult<View> {
    let view_layout: ViewLayout = params.layout.clone().into();
    let handler = self.get_handler(&view_layout)?;
    let user_id = self.user.user_id()?;
    let meta = params.meta.clone();
    match params.initial_data.is_empty() {
      true => {
        tracing::trace!("Create view with build-in data");
        handler
          .create_built_in_view(user_id, &params.view_id, &params.name, view_layout.clone())
          .await?;
      },
      false => {
        tracing::trace!("Create view with view data");
        handler
          .create_view_with_view_data(
            user_id,
            &params.view_id,
            &params.name,
            params.initial_data.clone(),
            view_layout.clone(),
            meta,
          )
          .await?;
      },
    }
    let view = create_view(params, view_layout);
    self.with_folder((), |folder| {
      folder.insert_view(view.clone());
    });

    notify_parent_view_did_change(self.mutex_folder.clone(), vec![view.parent_view_id.clone()]);
    Ok(view)
  }

  #[tracing::instrument(level = "debug", skip(self), err)]
  pub(crate) async fn close_view(&self, view_id: &str) -> Result<(), FlowyError> {
    let view = self
      .with_folder(None, |folder| folder.views.get_view(view_id))
      .ok_or_else(|| {
        FlowyError::record_not_found().context("Can't find the view when closing the view")
      })?;
    let handler = self.get_handler(&view.layout)?;
    handler.close_view(view_id).await?;
    Ok(())
  }

  pub async fn create_view_with_data(
    &self,
    view_id: &str,
    name: &str,
    view_layout: ViewLayout,
    data: Vec<u8>,
  ) -> FlowyResult<()> {
    let user_id = self.user.user_id()?;
    let handler = self.get_handler(&view_layout)?;
    handler
      .create_view_with_view_data(
        user_id,
        view_id,
        name,
        data,
        view_layout,
        HashMap::default(),
      )
      .await?;
    Ok(())
  }

  /// Returns the view with the given view id.
  /// The child views of the view will only access the first. So if you want to get the child view's
  /// child view, you need to call this method again.
  #[tracing::instrument(level = "debug", skip(self, view_id), err)]
  pub async fn get_view(&self, view_id: &str) -> FlowyResult<ViewPB> {
    let view_id = view_id.to_string();
    let folder = self.mutex_folder.lock();
    let folder = folder.as_ref().ok_or_else(folder_not_init_error)?;
    let trash_ids = folder
      .trash
      .get_all_trash()
      .into_iter()
      .map(|trash| trash.id)
      .collect::<Vec<String>>();

    if trash_ids.contains(&view_id) {
      return Err(FlowyError::record_not_found());
    }

    match folder.views.get_view(&view_id) {
      None => Err(FlowyError::record_not_found()),
      Some(mut view) => {
        view.children.retain(|b| !trash_ids.contains(&b.id));
        let child_views = folder
          .views
          .get_views_belong_to(&view.id)
          .into_iter()
          .filter(|view| !trash_ids.contains(&view.id))
          .collect::<Vec<View>>();
        let view_pb = view_pb_with_child_views(view, child_views);
        Ok(view_pb)
      },
    }
  }

  #[tracing::instrument(level = "debug", skip(self, view_id), err)]
  pub async fn delete_view(&self, view_id: &str) -> FlowyResult<()> {
    self.with_folder((), |folder| folder.views.delete_views(vec![view_id]));
    Ok(())
  }

  /// Move the view to trash. If the view is the current view, then set the current view to empty.
  /// When the view is moved to trash, all the child views will be moved to trash as well.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn move_view_to_trash(&self, view_id: &str) -> FlowyResult<()> {
    self.with_folder((), |folder| {
      folder.trash.add_trash(vec![TrashRecord {
        id: view_id.to_string(),
        created_at: timestamp(),
      }]);

      if let Some(view) = folder.get_current_view() {
        if view == view_id {
          folder.set_current_view("");
        }
      }
    });

    Ok(())
  }

  /// Move the view from one position to another position.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub async fn move_view(&self, view_id: &str, from: usize, to: usize) -> FlowyResult<()> {
    let view = self.with_folder(None, |folder| {
      folder.move_view(view_id, from as u32, to as u32)
    });

    match view {
      None => tracing::error!("Couldn't find the view. It should not be empty"),
      Some(view) => {
        notify_parent_view_did_change(self.mutex_folder.clone(), vec![view.parent_view_id]);
      },
    }
    Ok(())
  }

  /// Return a list of views that belong to the given parent view id.
  #[tracing::instrument(level = "debug", skip(self, parent_view_id), err)]
  pub async fn get_views_belong_to(&self, parent_view_id: &str) -> FlowyResult<Vec<View>> {
    let views = self.with_folder(vec![], |folder| {
      folder.views.get_views_belong_to(parent_view_id)
    });
    Ok(views)
  }

  /// Update the view with the given params.
  #[tracing::instrument(level = "trace", skip(self), err)]
  pub async fn update_view_with_params(&self, params: UpdateViewParams) -> FlowyResult<()> {
    let value = self.with_folder(None, |folder| {
      let old_view = folder.views.get_view(&params.view_id);
      let new_view = folder.views.update_view(&params.view_id, |update| {
        update
          .set_name_if_not_none(params.name)
          .set_desc_if_not_none(params.desc)
          .set_layout_if_not_none(params.layout)
          .done()
      });
      Some((old_view, new_view))
    });

    if let Some((Some(old_view), Some(new_view))) = value {
      if let Ok(handler) = self.get_handler(&old_view.layout) {
        handler.did_update_view(&old_view, &new_view).await?;
      }
    }

    if let Ok(view_pb) = self.get_view(&params.view_id).await {
      notify_parent_view_did_change(
        self.mutex_folder.clone(),
        vec![view_pb.parent_view_id.clone()],
      );
      send_notification(&view_pb.id, FolderNotification::DidUpdateView)
        .payload(view_pb)
        .send();
    }
    Ok(())
  }

  /// Duplicate the view with the given view id.
  #[tracing::instrument(level = "debug", skip(self), err)]
  pub(crate) async fn duplicate_view(&self, view_id: &str) -> Result<(), FlowyError> {
    let view = self
      .with_folder(None, |folder| folder.views.get_view(view_id))
      .ok_or_else(|| FlowyError::record_not_found().context("Can't duplicate the view"))?;

    let handler = self.get_handler(&view.layout)?;
    let view_data = handler.duplicate_view(&view.id).await?;
    let duplicate_params = CreateViewParams {
      parent_view_id: view.parent_view_id.clone(),
      name: format!("{} (copy)", &view.name),
      desc: view.desc,
      layout: view.layout.into(),
      initial_data: view_data.to_vec(),
      view_id: gen_view_id(),
      meta: Default::default(),
      set_as_current: true,
    };

    let _ = self.create_view_with_params(duplicate_params).await?;
    Ok(())
  }

  #[tracing::instrument(level = "trace", skip(self), err)]
  pub(crate) async fn set_current_view(&self, view_id: &str) -> Result<(), FlowyError> {
    let folder = self.mutex_folder.lock();
    let folder = folder.as_ref().ok_or_else(folder_not_init_error)?;
    folder.set_current_view(view_id);

    let workspace = folder.get_current_workspace();
    let view = folder
      .get_current_view()
      .and_then(|view_id| folder.views.get_view(&view_id));
    send_workspace_setting_notification(workspace, view);
    Ok(())
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn get_current_view(&self) -> Option<ViewPB> {
    let view_id = self.with_folder(None, |folder| folder.get_current_view())?;
    self.get_view(&view_id).await.ok()
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn get_all_trash(&self) -> Vec<TrashInfo> {
    self.with_folder(vec![], |folder| folder.trash.get_all_trash())
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn restore_all_trash(&self) {
    self.with_folder((), |folder| {
      folder.trash.clear();
    });

    send_notification("trash", FolderNotification::DidUpdateTrash)
      .payload(RepeatedTrashPB { items: vec![] })
      .send();
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn restore_trash(&self, trash_id: &str) {
    self.with_folder((), |folder| {
      folder.trash.delete_trash(vec![trash_id]);
    });
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn delete_trash(&self, trash_id: &str) {
    self.with_folder((), |folder| {
      folder.trash.delete_trash(vec![trash_id]);
      folder.views.delete_views(vec![trash_id]);
    })
  }

  #[tracing::instrument(level = "trace", skip(self))]
  pub(crate) async fn delete_all_trash(&self) {
    self.with_folder((), |folder| {
      let trash = folder.trash.get_all_trash();
      folder.trash.clear();
      folder.views.delete_views(trash);
    });

    send_notification("trash", FolderNotification::DidUpdateTrash)
      .payload(RepeatedTrashPB { items: vec![] })
      .send();
  }

  pub(crate) async fn import(&self, import_data: ImportParams) -> FlowyResult<View> {
    if import_data.data.is_none() && import_data.file_path.is_none() {
      return Err(FlowyError::new(
        ErrorCode::InvalidData,
        "data or file_path is required",
      ));
    }

    let handler = self.get_handler(&import_data.view_layout)?;
    let view_id = gen_view_id();
    if let Some(data) = import_data.data {
      handler
        .import_from_bytes(&view_id, &import_data.name, data)
        .await?;
    }

    if let Some(file_path) = import_data.file_path {
      handler
        .import_from_file_path(&view_id, &import_data.name, file_path)
        .await?;
    }

    let params = CreateViewParams {
      parent_view_id: import_data.parent_view_id,
      name: import_data.name,
      desc: "".to_string(),
      layout: import_data.view_layout.clone().into(),
      initial_data: vec![],
      view_id,
      meta: Default::default(),
      set_as_current: false,
    };

    let view = create_view(params, import_data.view_layout);
    self.with_folder((), |folder| {
      folder.insert_view(view.clone());
    });
    notify_parent_view_did_change(self.mutex_folder.clone(), vec![view.parent_view_id.clone()]);
    Ok(view)
  }

  /// Returns a handler that implements the [FolderOperationHandler] trait
  fn get_handler(
    &self,
    view_layout: &ViewLayout,
  ) -> FlowyResult<Arc<dyn FolderOperationHandler + Send + Sync>> {
    match self.operation_handlers.get(view_layout) {
      None => Err(FlowyError::internal().context(format!(
        "Get data processor failed. Unknown layout type: {:?}",
        view_layout
      ))),
      Some(processor) => Ok(processor.clone()),
    }
  }
}

/// Listen on the [ViewChange] after create/delete/update events happened
fn listen_on_view_change(mut rx: ViewChangeReceiver, weak_mutex_folder: &Weak<MutexFolder>) {
  let weak_mutex_folder = weak_mutex_folder.clone();
  tokio::spawn(async move {
    while let Ok(value) = rx.recv().await {
      if let Some(folder) = weak_mutex_folder.upgrade() {
        tracing::trace!("Did receive view change: {:?}", value);
        match value {
          ViewChange::DidCreateView { view } => {
            notify_parent_view_did_change(folder.clone(), vec![view.parent_view_id]);
          },
          ViewChange::DidDeleteView { views: _ } => {},
          ViewChange::DidUpdate { view } => {
            notify_parent_view_did_change(folder.clone(), vec![view.parent_view_id]);
          },
        };
      }
    }
  });
}

fn listen_on_folder_state_change(
  workspace_id: String,
  mut folder_state_rx: WatchStream<CollabState>,
  weak_mutex_folder: &Weak<MutexFolder>,
) {
  let weak_mutex_folder = weak_mutex_folder.clone();
  tokio::spawn(async move {
    while let Some(state) = folder_state_rx.next().await {
      if state.is_root_changed() {
        if let Some(mutex_folder) = weak_mutex_folder.upgrade() {
          let folder = mutex_folder.lock().take();
          if let Some(folder) = folder {
            tracing::trace!("🔥Reload folder");
            let reload_folder = folder.reload();
            notify_did_update_workspace(&workspace_id, &reload_folder);
            *mutex_folder.lock() = Some(reload_folder);
          }
        }
      }
    }
  });
}

/// Listen on the [TrashChange]s and notify the frontend some views were changed.
fn listen_on_trash_change(mut rx: TrashChangeReceiver, weak_mutex_folder: &Weak<MutexFolder>) {
  let weak_mutex_folder = weak_mutex_folder.clone();
  tokio::spawn(async move {
    while let Ok(value) = rx.recv().await {
      if let Some(folder) = weak_mutex_folder.upgrade() {
        let mut unique_ids = HashSet::new();
        tracing::trace!("Did receive trash change: {:?}", value);
        let ids = match value {
          TrashChange::DidCreateTrash { ids } => ids,
          TrashChange::DidDeleteTrash { ids } => ids,
        };

        if let Some(folder) = folder.lock().as_ref() {
          let views = folder.views.get_views(&ids);
          for view in views {
            unique_ids.insert(view.parent_view_id);
          }

          let repeated_trash: RepeatedTrashPB = folder.trash.get_all_trash().into();
          send_notification("trash", FolderNotification::DidUpdateTrash)
            .payload(repeated_trash)
            .send();
        }

        let parent_view_ids = unique_ids.into_iter().collect();
        notify_parent_view_did_change(folder.clone(), parent_view_ids);
      }
    }
  });
}

fn get_workspace_view_pbs(workspace_id: &str, folder: &Folder) -> Vec<ViewPB> {
  let trash_ids = folder
    .trash
    .get_all_trash()
    .into_iter()
    .map(|trash| trash.id)
    .collect::<Vec<String>>();

  let mut views = folder.get_workspace_views(workspace_id);
  views.retain(|view| !trash_ids.contains(&view.id));

  views
    .into_iter()
    .map(|view| {
      // Get child views
      let child_views = folder
        .views
        .get_views_belong_to(&view.id)
        .into_iter()
        .collect();
      view_pb_with_child_views(view, child_views)
    })
    .collect()
}

fn notify_did_update_workspace(workspace_id: &str, folder: &Folder) {
  let repeated_view: RepeatedViewPB = get_workspace_view_pbs(workspace_id, folder).into();
  tracing::trace!("Did update workspace views: {:?}", repeated_view);
  send_notification(workspace_id, FolderNotification::DidUpdateWorkspaceViews)
    .payload(repeated_view)
    .send();
}

/// Notify the the list of parent view ids that its child views were changed.
#[tracing::instrument(level = "debug", skip(folder, parent_view_ids))]
fn notify_parent_view_did_change<T: AsRef<str>>(
  folder: Arc<MutexFolder>,
  parent_view_ids: Vec<T>,
) -> Option<()> {
  let folder = folder.lock();
  let folder = folder.as_ref()?;
  let workspace_id = folder.get_current_workspace_id()?;
  let trash_ids = folder
    .trash
    .get_all_trash()
    .into_iter()
    .map(|trash| trash.id)
    .collect::<Vec<String>>();

  for parent_view_id in parent_view_ids {
    let parent_view_id = parent_view_id.as_ref();

    // if the view's parent id equal to workspace id. Then it will fetch the current
    // workspace views. Because the the workspace is not a view stored in the views map.
    if parent_view_id == workspace_id {
      notify_did_update_workspace(&workspace_id, folder)
    } else {
      // Parent view can contain a list of child views. Currently, only get the first level
      // child views.
      let parent_view = folder.views.get_view(parent_view_id)?;
      let mut child_views = folder.views.get_views_belong_to(parent_view_id);
      child_views.retain(|view| !trash_ids.contains(&view.id));
      event!(Level::DEBUG, child_views_count = child_views.len());

      // Post the notification
      let parent_view_pb = view_pb_with_child_views(parent_view, child_views);
      send_notification(parent_view_id, FolderNotification::DidUpdateChildViews)
        .payload(parent_view_pb)
        .send();
    }
  }

  None
}

fn folder_not_init_error() -> FlowyError {
  FlowyError::internal().context("Folder not initialized")
}

#[derive(Clone, Default)]
pub struct MutexFolder(Arc<Mutex<Option<Folder>>>);
impl Deref for MutexFolder {
  type Target = Arc<Mutex<Option<Folder>>>;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}
unsafe impl Sync for MutexFolder {}
unsafe impl Send for MutexFolder {}
