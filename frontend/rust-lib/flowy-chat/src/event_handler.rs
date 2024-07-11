use flowy_chat_pub::cloud::ChatMessageType;
use std::path::PathBuf;

use allo_isolate::Isolate;
use std::sync::{Arc, Weak};
use tokio::sync::oneshot;
use validator::Validate;

use crate::chat_manager::ChatManager;
use crate::entities::*;
use crate::local_ai::local_llm_chat::LLMModelInfo;
use crate::tools::AITools;
use flowy_error::{FlowyError, FlowyResult};
use lib_dispatch::prelude::{data_result_ok, AFPluginData, AFPluginState, DataResult};
use lib_infra::isolate_stream::IsolateSink;

fn upgrade_chat_manager(
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> FlowyResult<Arc<ChatManager>> {
  let chat_manager = chat_manager
    .upgrade()
    .ok_or(FlowyError::internal().with_context("The chat manager is already dropped"))?;
  Ok(chat_manager)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn stream_chat_message_handler(
  data: AFPluginData<StreamChatPayloadPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<ChatMessagePB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let data = data.into_inner();
  data.validate()?;

  let message_type = match data.message_type {
    ChatMessageTypePB::System => ChatMessageType::System,
    ChatMessageTypePB::User => ChatMessageType::User,
  };

  let question = chat_manager
    .stream_chat_message(
      &data.chat_id,
      &data.message,
      message_type,
      data.text_stream_port,
    )
    .await?;
  data_result_ok(question)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn load_prev_message_handler(
  data: AFPluginData<LoadPrevChatMessagePB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<ChatMessageListPB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let data = data.into_inner();
  data.validate()?;

  let messages = chat_manager
    .load_prev_chat_messages(&data.chat_id, data.limit, data.before_message_id)
    .await?;
  data_result_ok(messages)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn load_next_message_handler(
  data: AFPluginData<LoadNextChatMessagePB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<ChatMessageListPB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let data = data.into_inner();
  data.validate()?;

  let messages = chat_manager
    .load_latest_chat_messages(&data.chat_id, data.limit, data.after_message_id)
    .await?;
  data_result_ok(messages)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn get_related_question_handler(
  data: AFPluginData<ChatMessageIdPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<RepeatedRelatedQuestionPB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let data = data.into_inner();
  let messages = chat_manager
    .get_related_questions(&data.chat_id, data.message_id)
    .await?;
  data_result_ok(messages)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn get_answer_handler(
  data: AFPluginData<ChatMessageIdPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<ChatMessagePB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let data = data.into_inner();
  let (tx, rx) = tokio::sync::oneshot::channel();
  tokio::spawn(async move {
    let message = chat_manager
      .generate_answer(&data.chat_id, data.message_id)
      .await?;
    let _ = tx.send(message);
    Ok::<_, FlowyError>(())
  });
  let message = rx.await?;
  data_result_ok(message)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn stop_stream_handler(
  data: AFPluginData<StopStreamPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> Result<(), FlowyError> {
  let data = data.into_inner();
  data.validate()?;

  let chat_manager = upgrade_chat_manager(chat_manager)?;
  chat_manager.stop_stream(&data.chat_id).await?;
  Ok(())
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn get_local_ai_model_info_handler(
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<LLMModelInfoPB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let (tx, rx) = oneshot::channel::<Result<LLMModelInfo, FlowyError>>();
  tokio::spawn(async move {
    let model_info = chat_manager.llm_controller.model_info().await;
    let _ = tx.send(model_info);
  });

  let model_info = rx.await??;
  data_result_ok(model_info.into())
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn update_local_llm_model_handler(
  data: AFPluginData<LLMModelPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<LocalModelStatePB, FlowyError> {
  let data = data.into_inner();
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let state = chat_manager
    .llm_controller
    .use_local_llm(data.llm_id)
    .await?;
  data_result_ok(state)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn get_local_llm_state_handler(
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<LocalModelStatePB, FlowyError> {
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let state = chat_manager.llm_controller.get_local_llm_state().await?;
  data_result_ok(state)
}

pub(crate) async fn start_complete_text_handler(
  data: AFPluginData<CompleteTextPB>,
  tools: AFPluginState<Arc<AITools>>,
) -> DataResult<CompleteTextTaskPB, FlowyError> {
  let task = tools.create_complete_task(data.into_inner()).await?;
  data_result_ok(task)
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn stop_complete_text_handler(
  data: AFPluginData<CompleteTextTaskPB>,
  tools: AFPluginState<Arc<AITools>>,
) -> Result<(), FlowyError> {
  let data = data.into_inner();
  tools.cancel_complete_task(&data.task_id).await;
  Ok(())
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn chat_file_handler(
  data: AFPluginData<ChatFilePB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> Result<(), FlowyError> {
  let data = data.try_into_inner()?;
  let file_path = PathBuf::from(&data.file_path);
  let (tx, rx) = oneshot::channel::<Result<(), FlowyError>>();
  tokio::spawn(async move {
    let chat_manager = upgrade_chat_manager(chat_manager)?;
    chat_manager
      .chat_with_file(&data.chat_id, file_path)
      .await?;
    let _ = tx.send(Ok(()));
    Ok::<_, FlowyError>(())
  });

  rx.await?
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn download_llm_resource_handler(
  data: AFPluginData<DownloadLLMPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> DataResult<DownloadTaskPB, FlowyError> {
  let data = data.try_into_inner()?;
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  let text_sink = IsolateSink::new(Isolate::new(data.progress_stream));
  let task_id = chat_manager.llm_controller.start_downloading(text_sink)?;
  data_result_ok(DownloadTaskPB { task_id })
}

#[tracing::instrument(level = "debug", skip_all, err)]
pub(crate) async fn cancel_download_llm_resource_handler(
  data: AFPluginData<DownloadTaskPB>,
  chat_manager: AFPluginState<Weak<ChatManager>>,
) -> Result<(), FlowyError> {
  let data = data.into_inner();
  let chat_manager = upgrade_chat_manager(chat_manager)?;
  chat_manager.llm_controller.cancel_download(&data.task_id)?;
  Ok(())
}
