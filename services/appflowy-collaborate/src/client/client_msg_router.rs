use std::sync::Arc;
use std::time::Duration;

use access_control::collab::RealtimeAccessControl;
use async_trait::async_trait;
use collab_rt_entity::user::RealtimeUser;
use collab_rt_entity::ClientCollabMessage;
use collab_rt_entity::{MessageByObjectId, RealtimeMessage};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tokio_stream::StreamExt;
use tracing::{error, trace};
use uuid::Uuid;

use crate::util::channel_ext::UnboundedSenderSink;

#[async_trait]
pub trait RealtimeClientWebsocketSink: Send + Sync + 'static {
  fn do_send(&self, message: RealtimeMessage);
}

/// Manages message routing for client connections in a collaborative environment.
///
/// acts as an intermediary that receives messages from individual client sessions and
/// forwards them to the appropriate destination, either by broadcasting to all connected
/// clients or directing them to a specific client. It leverages a websocket sink for outgoing
/// messages and a broadcast channel to receive incoming messages from the collab server.
pub struct ClientMessageRouter {
  pub(crate) sink: Arc<dyn RealtimeClientWebsocketSink>,
  /// Used to receive messages from the collab server. The message will forward to the [CollabBroadcast] which
  /// will broadcast the message to all connected clients.
  ///
  /// The message flow:
  /// ClientSession(websocket) -> [CollabRealtimeServer] -> [ClientMessageRouter] -> [CollabBroadcast] 1->* websocket(client)
  pub(crate) stream_tx: tokio::sync::broadcast::Sender<MessageByObjectId>,
}

impl ClientMessageRouter {
  pub fn new(sink: impl RealtimeClientWebsocketSink) -> Self {
    // When receive a new connection, create a new [ClientStream] that holds the connection's websocket
    let (stream_tx, _) = tokio::sync::broadcast::channel(1000);
    Self {
      sink: Arc::new(sink),
      stream_tx,
    }
  }

  /// Initializes a communication channel for a client and a specific collaboration object.
  ///
  /// sets up a two-way communication channel between the client and the collaboration server,
  /// tailored for a specific object identified by `object_id`. It establishes a mechanism for sending changes to
  /// and receiving updates from the collaboration object, ensuring that only authorized changes are communicated.
  ///
  /// - An `UnboundedSenderSink<T>`, which is used to send updates to the connected client.
  /// - A `ReceiverStream<MessageByObjectId>`, which is used to receive authorized updates from the connected client.
  ///
  pub fn init_client_communication<T>(
    &mut self,
    workspace_id: Uuid,
    user: &RealtimeUser,
    object_id: Uuid,
    access_control: Arc<dyn RealtimeAccessControl>,
  ) -> (UnboundedSenderSink<T>, ReceiverStream<MessageByObjectId>)
  where
    T: Into<RealtimeMessage> + Send + Sync + 'static,
  {
    let client_ws_sink = self.sink.clone();
    let mut stream_rx = BroadcastStream::new(self.stream_tx.subscribe());

    // Send the message to the connected websocket client. When the client receive the message,
    // it will apply the changes.
    let (client_sink_tx, mut client_sink_rx) = tokio::sync::mpsc::unbounded_channel::<T>();
    let sink_access_control = access_control.clone();
    let uid = user.uid;
    let client_sink = UnboundedSenderSink::<T>::new(client_sink_tx);
    tokio::spawn(async move {
      while let Some(msg) = client_sink_rx.recv().await {
        let result = sink_access_control
          .can_read_collab(&workspace_id, &uid, &object_id)
          .await;
        match result {
          Ok(is_allowed) => {
            if is_allowed {
              let rt_msg = msg.into();
              client_ws_sink.do_send(rt_msg);
            } else {
              trace!("user:{} is not allowed to read {}", uid, object_id);
              tokio::time::sleep(Duration::from_secs(2)).await;
            }
          },
          Err(err) => {
            error!("user:{} fail to receive updates: {}", uid, err);
            tokio::time::sleep(Duration::from_secs(1)).await;
          },
        }
      }
    });
    let user = user.clone();
    // stream_rx continuously receive messages from the websocket client and then
    // forward the message to the subscriber which is the broadcast channel [CollabBroadcast].
    let (client_msg_rx, rx) = tokio::sync::mpsc::channel(100);
    let client_stream = ReceiverStream::new(rx);
    tokio::spawn(async move {
      let target_object_id = object_id.to_string();
      while let Some(Ok(messages_by_oid)) = stream_rx.next().await {
        for (message_object_id, original_messages) in messages_by_oid.into_inner() {
          // if the message is not for the target object, skip it. The stream_rx receives different
          // objects' messages, so we need to filter out the messages that are not for the target object.
          if target_object_id != message_object_id {
            continue;
          }

          // before applying user messages, we need to check if the user has the permission
          // valid_messages contains the messages that the user is allowed to apply
          // invalid_message contains the messages that the user is not allowed to apply
          let (valid_messages, _) = Self::access_control(
            &workspace_id,
            &user.uid,
            &object_id,
            access_control.clone(),
            original_messages,
          )
          .await;
          if valid_messages.is_empty() {
            continue;
          }

          // if tx.send return error, it means the client is disconnected from the group
          if let Err(err) = client_msg_rx
            .send(MessageByObjectId::new_with_message(
              message_object_id,
              valid_messages,
            ))
            .await
          {
            trace!(
              "{} send message to user:{} stream fail with error: {}, break the loop",
              target_object_id,
              user.user_device(),
              err,
            );
            return;
          }
        }
      }
    });
    (client_sink, client_stream)
  }

  pub async fn send_message(&self, message: RealtimeMessage) {
    self.sink.do_send(message);
  }

  #[inline]
  async fn access_control(
    workspace_id: &Uuid,
    uid: &i64,
    object_id: &Uuid,
    access_control: Arc<dyn RealtimeAccessControl>,
    messages: Vec<ClientCollabMessage>,
  ) -> (Vec<ClientCollabMessage>, Vec<ClientCollabMessage>) {
    let can_write = access_control
      .can_write_collab(workspace_id, uid, object_id)
      .await
      .unwrap_or(false);

    let mut valid_messages = Vec::with_capacity(messages.len());
    let mut invalid_messages = Vec::with_capacity(messages.len());

    for message in messages {
      if can_write {
        valid_messages.push(message);
      } else {
        invalid_messages.push(message);
      }
    }
    (valid_messages, invalid_messages)
  }
}
