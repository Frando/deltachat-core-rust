//! # Messages and their identifiers

use async_std::path::{Path, PathBuf};
use async_std::prelude::*;
use deltachat_derive::*;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use sqlx::query::QueryAs;

use crate::chat::{self, Chat, ChatId};
use crate::constants::*;
use crate::contact::*;
use crate::context::*;
use crate::dc_tools::*;
use crate::error::{ensure, Error};
use crate::events::Event;
use crate::job::{self, Action};
use crate::lot::{Lot, LotState, Meaning};
use crate::mimeparser::SystemMessage;
use crate::param::*;
use crate::pgp::*;
use crate::stock::StockMessage;

lazy_static! {
    static ref UNWRAP_RE: regex::Regex = regex::Regex::new(r"\s+").unwrap();
}

// In practice, the user additionally cuts the string themselves
// pixel-accurate.
const SUMMARY_CHARACTERS: usize = 160;

/// Message ID, including reserved IDs.
///
/// Some message IDs are reserved to identify special message types.
/// This type can represent both the special as well as normal
/// messages.
#[derive(
    Debug,
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    ToPrimitive,
    FromPrimitive,
    Sqlx,
)]
pub struct MsgId(u32);

impl MsgId {
    /// Create a new [MsgId].
    pub fn new(id: u32) -> MsgId {
        MsgId(id)
    }

    /// Create a new unset [MsgId].
    pub fn new_unset() -> MsgId {
        MsgId(0)
    }

    /// Whether the message ID signifies a special message.
    ///
    /// This kind of message ID can not be used for real messages.
    pub fn is_special(self) -> bool {
        self.0 <= DC_MSG_ID_LAST_SPECIAL
    }

    /// Whether the message ID is unset.
    ///
    /// When a message is created it initially has a ID of `0`, which
    /// is filled in by a real message ID once the message is saved in
    /// the database.  This returns true while the message has not
    /// been saved and thus not yet been given an actual message ID.
    ///
    /// When this is `true`, [MsgId::is_special] will also always be
    /// `true`.
    pub fn is_unset(self) -> bool {
        self.0 == 0
    }

    /// Whether the message ID is the special marker1 marker.
    ///
    /// See the docs of the `dc_get_chat_msgs` C API for details.
    pub fn is_marker1(self) -> bool {
        self.0 == DC_MSG_ID_MARKER1
    }

    /// Whether the message ID is the special day marker.
    ///
    /// See the docs of the `dc_get_chat_msgs` C API for details.
    pub fn is_daymarker(self) -> bool {
        self.0 == DC_MSG_ID_DAYMARKER
    }

    /// Put message into trash chat and delete message text.
    ///
    /// It means the message is deleted locally, but not on the server
    /// yet.
    pub async fn trash(self, context: &Context) -> crate::sql::Result<()> {
        let chat_id = ChatId::new(DC_CHAT_ID_TRASH);
        context
            .sql
            .execute(
                "UPDATE msgs SET chat_id=?, txt='', txt_raw='' WHERE id=?",
                paramsx![chat_id, self],
            )
            .await?;

        Ok(())
    }

    /// Deletes a message and corresponding MDNs from the database.
    pub async fn delete_from_db(self, context: &Context) -> crate::sql::Result<()> {
        // We don't use transactions yet, so remove MDNs first to make
        // sure they are not left while the message is deleted.
        context
            .sql
            .execute("DELETE FROM msgs_mdns WHERE msg_id=?;", paramsx![self])
            .await?;
        context
            .sql
            .execute("DELETE FROM msgs WHERE id=?;", paramsx![self])
            .await?;
        Ok(())
    }

    /// Removes IMAP server UID and folder from the database record.
    ///
    /// It is used to avoid trying to remove the message from the
    /// server multiple times when there are multiple message records
    /// pointing to the same server UID.
    pub(crate) async fn unlink(self, context: &Context) -> crate::sql::Result<()> {
        context
            .sql
            .execute(
                r#"
UPDATE msgs
  SET server_folder='', server_uid=0
  WHERE id=?
"#,
                paramsx![self],
            )
            .await?;
        Ok(())
    }

    /// Bad evil escape hatch.
    ///
    /// Avoid using this, eventually types should be cleaned up enough
    /// that it is no longer necessary.
    pub fn to_u32(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for MsgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Would be nice if we could use match here, but no computed values in ranges.
        if self.0 == DC_MSG_ID_MARKER1 {
            write!(f, "Msg#Marker1")
        } else if self.0 == DC_MSG_ID_DAYMARKER {
            write!(f, "Msg#DayMarker")
        } else if self.0 <= DC_MSG_ID_LAST_SPECIAL {
            write!(f, "Msg#UnknownSpecial")
        } else {
            write!(f, "Msg#{}", self.0)
        }
    }
}

/// Message ID was invalid.
///
/// This usually occurs when trying to use a message ID of
/// [DC_MSG_ID_LAST_SPECIAL] or below in a situation where this is not
/// possible.
#[derive(Debug, thiserror::Error)]
#[error("Invalid Message ID.")]
pub struct InvalidMsgId;

#[derive(
    Debug, Copy, Clone, PartialEq, FromPrimitive, ToPrimitive, Serialize, Deserialize, Sqlx,
)]
#[repr(u8)]
pub(crate) enum MessengerMessage {
    No = 0,
    Yes = 1,

    /// No, but reply to messenger message.
    Reply = 2,
}

impl Default for MessengerMessage {
    fn default() -> Self {
        Self::No
    }
}

/// An object representing a single message in memory.
/// The message object is not updated.
/// If you want an update, you have to recreate the object.
///
/// to check if a mail was sent, use dc_msg_is_sent()
/// approx. max. length returned by dc_msg_get_text()
/// approx. max. length returned by dc_get_msg_info()
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    pub(crate) id: MsgId,
    pub(crate) from_id: u32,
    pub(crate) to_id: u32,
    pub(crate) chat_id: ChatId,
    pub(crate) viewtype: Viewtype,
    pub(crate) state: MessageState,
    pub(crate) hidden: bool,
    pub(crate) timestamp_sort: i64,
    pub(crate) timestamp_sent: i64,
    pub(crate) timestamp_rcvd: i64,
    pub(crate) text: Option<String>,
    pub(crate) rfc724_mid: String,
    pub(crate) in_reply_to: Option<String>,
    pub(crate) server_folder: Option<String>,
    pub(crate) server_uid: u32,
    pub(crate) is_dc_message: MessengerMessage,
    pub(crate) starred: bool,
    pub(crate) chat_blocked: Blocked,
    pub(crate) location_id: u32,
    pub(crate) param: Params,
}

impl<'a> sqlx::FromRow<'a, sqlx::sqlite::SqliteRow> for Message {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;

        let id = row.try_get("id")?;

        let text;
        if let Some(buf) = row.try_get::<Option<&[u8]>, _>("txt")? {
            if let Ok(t) = String::from_utf8(buf.to_vec()) {
                text = t;
            } else {
                eprintln!(
                    "dc_msg_load_from_db: could not get text column as non-lossy utf8 id {}",
                    id
                );
                text = String::from_utf8_lossy(buf).into_owned();
            }
        } else {
            text = "".to_string();
        }

        Ok(Message {
            id,
            rfc724_mid: row.try_get::<String, _>("rfc724mid")?,
            in_reply_to: row.try_get::<Option<String>, _>("mime_in_reply_to")?,
            server_folder: row.try_get::<Option<String>, _>("server_folder")?,
            server_uid: row.try_get::<i64, _>("server_uid")? as u32,
            chat_id: row.try_get("chat_id")?,
            from_id: row.try_get::<i64, _>("from_id")? as u32,
            to_id: row.try_get::<i64, _>("to_id")? as u32,
            timestamp_sort: row.try_get("timestamp")?,
            timestamp_sent: row.try_get("timestamp_sent")?,
            timestamp_rcvd: row.try_get("timestamp_rcvd")?,
            viewtype: row.try_get("type")?,
            state: row.try_get("state")?,
            is_dc_message: row.try_get("msgrmsg")?,
            text: Some(text),
            param: row
                .try_get::<String, _>("param")?
                .parse()
                .unwrap_or_default(),
            starred: row.try_get("starred")?,
            hidden: row.try_get("hidden")?,
            location_id: row.try_get::<i64, _>("location")? as u32,
            chat_blocked: row
                .try_get::<Option<Blocked>, _>("blocked")?
                .unwrap_or_default(),
        })
    }
}

impl Message {
    pub fn new(viewtype: Viewtype) -> Self {
        let mut msg = Message::default();
        msg.viewtype = viewtype;

        msg
    }

    pub async fn load_from_db(context: &Context, id: MsgId) -> Result<Message, Error> {
        ensure!(
            !id.is_special(),
            "Can not load special message IDs from DB."
        );
        let msg: Message = context
            .sql
            .query_row(
                concat!(
                    "SELECT",
                    "    m.id AS id,",
                    "    rfc724_mid AS rfc724mid,",
                    "    m.mime_in_reply_to AS mime_in_reply_to,",
                    "    m.server_folder AS server_folder,",
                    "    m.server_uid AS server_uid,",
                    "    m.chat_id AS chat_id,",
                    "    m.from_id AS from_id,",
                    "    m.to_id AS to_id,",
                    "    m.timestamp AS timestamp,",
                    "    m.timestamp_sent AS timestamp_sent,",
                    "    m.timestamp_rcvd AS timestamp_rcvd,",
                    "    m.type AS type,",
                    "    m.state AS state,",
                    "    m.msgrmsg AS msgrmsg,",
                    "    m.txt AS txt,",
                    "    m.param AS param,",
                    "    m.starred AS starred,",
                    "    m.hidden AS hidden,",
                    "    m.location_id AS location,",
                    "    c.blocked AS blocked",
                    " FROM msgs m LEFT JOIN chats c ON c.id=m.chat_id",
                    " WHERE m.id=?;"
                ),
                paramsx![id],
            )
            .await?;

        Ok(msg)
    }

    pub fn get_filemime(&self) -> Option<String> {
        if let Some(m) = self.param.get(Param::MimeType) {
            return Some(m.to_string());
        } else if let Some(file) = self.param.get(Param::File) {
            if let Some((_, mime)) = guess_msgtype_from_suffix(Path::new(file)) {
                return Some(mime.to_string());
            }
            // we have a file but no mimetype, let's use a generic one
            return Some("application/octet-stream".to_string());
        }
        // no mimetype and no file
        None
    }

    pub fn get_file(&self, context: &Context) -> Option<PathBuf> {
        self.param.get_path(Param::File, context).unwrap_or(None)
    }

    pub async fn try_calc_and_set_dimensions(&mut self, context: &Context) -> Result<(), Error> {
        if chat::msgtype_has_file(self.viewtype) {
            let file_param = self.param.get_path(Param::File, context)?;
            if let Some(path_and_filename) = file_param {
                if (self.viewtype == Viewtype::Image || self.viewtype == Viewtype::Gif)
                    && !self.param.exists(Param::Width)
                {
                    self.param.set_int(Param::Width, 0);
                    self.param.set_int(Param::Height, 0);

                    if let Ok(buf) = dc_read_file(context, path_and_filename).await {
                        if let Ok((width, height)) = dc_get_filemeta(&buf) {
                            self.param.set_int(Param::Width, width as i32);
                            self.param.set_int(Param::Height, height as i32);
                        }
                    }

                    if !self.id.is_unset() {
                        self.save_param_to_disk(context).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Check if a message has a location bound to it.
    /// These messages are also returned by dc_get_locations()
    /// and the UI may decide to display a special icon beside such messages,
    ///
    /// @memberof Message
    /// @param msg The message object.
    /// @return 1=Message has location bound to it, 0=No location bound to message.
    pub fn has_location(&self) -> bool {
        self.location_id != 0
    }

    /// Set any location that should be bound to the message object.
    /// The function is useful to add a marker to the map
    /// at a position different from the self-location.
    /// You should not call this function
    /// if you want to bind the current self-location to a message;
    /// this is done by dc_set_location() and dc_send_locations_to_chat().
    ///
    /// Typically results in the event #DC_EVENT_LOCATION_CHANGED with
    /// contact_id set to DC_CONTACT_ID_SELF.
    ///
    /// @param latitude North-south position of the location.
    /// @param longitude East-west position of the location.
    pub fn set_location(&mut self, latitude: f64, longitude: f64) {
        if latitude == 0.0 && longitude == 0.0 {
            return;
        }

        self.param.set_float(Param::SetLatitude, latitude);
        self.param.set_float(Param::SetLongitude, longitude);
    }

    pub fn get_timestamp(&self) -> i64 {
        if 0 != self.timestamp_sent {
            self.timestamp_sent
        } else {
            self.timestamp_sort
        }
    }

    pub fn get_id(&self) -> MsgId {
        self.id
    }

    pub fn get_from_id(&self) -> u32 {
        self.from_id
    }

    pub fn get_chat_id(&self) -> ChatId {
        if self.chat_blocked != Blocked::Not {
            ChatId::new(DC_CHAT_ID_DEADDROP)
        } else {
            self.chat_id
        }
    }

    pub fn get_viewtype(&self) -> Viewtype {
        self.viewtype
    }

    pub fn get_state(&self) -> MessageState {
        self.state
    }

    pub fn get_received_timestamp(&self) -> i64 {
        self.timestamp_rcvd
    }

    pub fn get_sort_timestamp(&self) -> i64 {
        self.timestamp_sort
    }

    pub fn get_text(&self) -> Option<String> {
        self.text
            .as_ref()
            .map(|text| dc_truncate(text, 30000).to_string())
    }

    pub fn get_filename(&self) -> Option<String> {
        self.param
            .get(Param::File)
            .and_then(|file| Path::new(file).file_name())
            .map(|name| name.to_string_lossy().to_string())
    }

    pub async fn get_filebytes(&self, context: &Context) -> u64 {
        match self.param.get_path(Param::File, context) {
            Ok(Some(path)) => dc_get_filebytes(context, &path).await,
            Ok(None) => 0,
            Err(_) => 0,
        }
    }

    pub fn get_width(&self) -> i32 {
        self.param.get_int(Param::Width).unwrap_or_default()
    }

    pub fn get_height(&self) -> i32 {
        self.param.get_int(Param::Height).unwrap_or_default()
    }

    pub fn get_duration(&self) -> i32 {
        self.param.get_int(Param::Duration).unwrap_or_default()
    }

    pub fn get_showpadlock(&self) -> bool {
        self.param.get_int(Param::GuaranteeE2ee).unwrap_or_default() != 0
    }

    pub async fn get_summary(&mut self, context: &Context, chat: Option<&Chat>) -> Lot {
        let mut ret = Lot::new();

        let chat_loaded: Chat;
        let chat = if let Some(chat) = chat {
            chat
        } else if let Ok(chat) = Chat::load_from_db(context, self.chat_id).await {
            chat_loaded = chat;
            &chat_loaded
        } else {
            return ret;
        };

        let contact = if self.from_id != DC_CONTACT_ID_SELF as u32
            && (chat.typ == Chattype::Group || chat.typ == Chattype::VerifiedGroup)
        {
            Contact::get_by_id(context, self.from_id).await.ok()
        } else {
            None
        };

        ret.fill(self, chat, contact.as_ref(), context).await;

        ret
    }

    pub async fn get_summarytext(&self, context: &Context, approx_characters: usize) -> String {
        get_summarytext_by_raw(
            self.viewtype,
            self.text.as_ref(),
            &self.param,
            approx_characters,
            context,
        )
        .await
    }

    pub fn has_deviating_timestamp(&self) -> bool {
        let cnv_to_local = dc_gm2local_offset();
        let sort_timestamp = self.get_sort_timestamp() as i64 + cnv_to_local;
        let send_timestamp = self.get_timestamp() as i64 + cnv_to_local;

        sort_timestamp / 86400 != send_timestamp / 86400
    }

    pub fn is_sent(&self) -> bool {
        self.state as i32 >= MessageState::OutDelivered as i32
    }

    pub fn is_starred(&self) -> bool {
        self.starred
    }

    pub fn is_forwarded(&self) -> bool {
        0 != self.param.get_int(Param::Forwarded).unwrap_or_default()
    }

    pub fn is_info(&self) -> bool {
        let cmd = self.param.get_cmd();
        self.from_id == DC_CONTACT_ID_INFO as u32
            || self.to_id == DC_CONTACT_ID_INFO as u32
            || cmd != SystemMessage::Unknown && cmd != SystemMessage::AutocryptSetupMessage
    }

    /// Whether the message is still being created.
    ///
    /// Messages with attachments might be created before the
    /// attachment is ready.  In this case some more restrictions on
    /// the attachment apply, e.g. if the file to be attached is still
    /// being written to or otherwise will still change it can not be
    /// copied to the blobdir.  Thus those attachments need to be
    /// created immediately in the blobdir with a valid filename.
    pub fn is_increation(&self) -> bool {
        chat::msgtype_has_file(self.viewtype) && self.state == MessageState::OutPreparing
    }

    pub fn is_setupmessage(&self) -> bool {
        if self.viewtype != Viewtype::File {
            return false;
        }

        self.param.get_cmd() == SystemMessage::AutocryptSetupMessage
    }

    pub async fn get_setupcodebegin(&self, context: &Context) -> Option<String> {
        if !self.is_setupmessage() {
            return None;
        }

        if let Some(filename) = self.get_file(context) {
            if let Ok(ref buf) = dc_read_file(context, filename).await {
                if let Ok((typ, headers, _)) = split_armored_data(buf) {
                    if typ == pgp::armor::BlockType::Message {
                        return headers.get(crate::pgp::HEADER_SETUPCODE).cloned();
                    }
                }
            }
        }

        None
    }

    pub fn set_text(&mut self, text: Option<String>) {
        self.text = text;
    }

    pub fn set_file(&mut self, file: impl AsRef<str>, filemime: Option<&str>) {
        self.param.set(Param::File, file);
        if let Some(filemime) = filemime {
            self.param.set(Param::MimeType, filemime);
        }
    }

    pub fn set_dimension(&mut self, width: i32, height: i32) {
        self.param.set_int(Param::Width, width);
        self.param.set_int(Param::Height, height);
    }

    pub fn set_duration(&mut self, duration: i32) {
        self.param.set_int(Param::Duration, duration);
    }

    pub async fn latefiling_mediasize(
        &mut self,
        context: &Context,
        width: i32,
        height: i32,
        duration: i32,
    ) {
        if width > 0 && height > 0 {
            self.param.set_int(Param::Width, width);
            self.param.set_int(Param::Height, height);
        }
        if duration > 0 {
            self.param.set_int(Param::Duration, duration);
        }
        self.save_param_to_disk(context).await;
    }

    pub async fn save_param_to_disk(&mut self, context: &Context) -> bool {
        context
            .sql
            .execute(
                "UPDATE msgs SET param=? WHERE id=?;",
                paramsx![self.param.to_string(), self.id],
            )
            .await
            .is_ok()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    FromPrimitive,
    ToPrimitive,
    Serialize,
    Deserialize,
    sqlx::Type,
)]
#[repr(i32)]
pub enum MessageState {
    Undefined = 0,

    /// Incoming *fresh* message. Fresh messages are neither noticed
    /// nor seen and are typically shown in notifications.
    InFresh = 10,

    /// Incoming *noticed* message. E.g. chat opened but message not
    /// yet read - noticed messages are not counted as unread but did
    /// not marked as read nor resulted in MDNs.
    InNoticed = 13,

    /// Incoming message, really *seen* by the user. Marked as read on
    /// IMAP and MDN may be sent.
    InSeen = 16,

    /// For files which need time to be prepared before they can be
    /// sent, the message enters this state before
    /// OutPending.
    OutPreparing = 18,

    /// Message saved as draft.
    OutDraft = 19,

    /// The user has pressed the "send" button but the message is not
    /// yet sent and is pending in some way. Maybe we're offline (no
    /// checkmark).
    OutPending = 20,

    /// *Unrecoverable* error (*recoverable* errors result in pending
    /// messages).
    OutFailed = 24,

    /// Outgoing message successfully delivered to server (one
    /// checkmark). Note, that already delivered messages may get into
    /// the OutFailed state if we get such a hint from the server.
    OutDelivered = 26,

    /// Outgoing message read by the recipient (two checkmarks; this
    /// requires goodwill on the receiver's side)
    OutMdnRcvd = 28,
}

impl Default for MessageState {
    fn default() -> Self {
        MessageState::Undefined
    }
}

impl std::fmt::Display for MessageState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Undefined => "Undefined",
                Self::InFresh => "Fresh",
                Self::InNoticed => "Noticed",
                Self::InSeen => "Seen",
                Self::OutPreparing => "Preparing",
                Self::OutDraft => "Draft",
                Self::OutPending => "Pending",
                Self::OutFailed => "Failed",
                Self::OutDelivered => "Delivered",
                Self::OutMdnRcvd => "Read",
            }
        )
    }
}

impl From<MessageState> for LotState {
    fn from(s: MessageState) -> Self {
        use MessageState::*;
        match s {
            Undefined => LotState::Undefined,
            InFresh => LotState::MsgInFresh,
            InNoticed => LotState::MsgInNoticed,
            InSeen => LotState::MsgInSeen,
            OutPreparing => LotState::MsgOutPreparing,
            OutDraft => LotState::MsgOutDraft,
            OutPending => LotState::MsgOutPending,
            OutFailed => LotState::MsgOutFailed,
            OutDelivered => LotState::MsgOutDelivered,
            OutMdnRcvd => LotState::MsgOutMdnRcvd,
        }
    }
}

impl MessageState {
    pub fn can_fail(self) -> bool {
        match self {
            MessageState::OutPreparing | MessageState::OutPending | MessageState::OutDelivered => {
                true
            }
            _ => false,
        }
    }
}

impl Lot {
    /* library-internal */
    /* in practice, the user additionally cuts the string himself pixel-accurate */
    pub async fn fill(
        &mut self,
        msg: &mut Message,
        chat: &Chat,
        contact: Option<&Contact>,
        context: &Context,
    ) {
        if msg.state == MessageState::OutDraft {
            self.text1 = Some(
                context
                    .stock_str(StockMessage::Draft)
                    .await
                    .to_owned()
                    .into(),
            );
            self.text1_meaning = Meaning::Text1Draft;
        } else if msg.from_id == DC_CONTACT_ID_SELF {
            if msg.is_info() || chat.is_self_talk() {
                self.text1 = None;
                self.text1_meaning = Meaning::None;
            } else {
                self.text1 = Some(
                    context
                        .stock_str(StockMessage::SelfMsg)
                        .await
                        .to_owned()
                        .into(),
                );
                self.text1_meaning = Meaning::Text1Self;
            }
        } else if chat.typ == Chattype::Group || chat.typ == Chattype::VerifiedGroup {
            if msg.is_info() || contact.is_none() {
                self.text1 = None;
                self.text1_meaning = Meaning::None;
            } else {
                if chat.id.is_deaddrop() {
                    if let Some(contact) = contact {
                        self.text1 = Some(contact.get_display_name().into());
                    } else {
                        self.text1 = None;
                    }
                } else if let Some(contact) = contact {
                    self.text1 = Some(contact.get_first_name().into());
                } else {
                    self.text1 = None;
                }
                self.text1_meaning = Meaning::Text1Username;
            }
        }

        self.text2 = Some(
            get_summarytext_by_raw(
                msg.viewtype,
                msg.text.as_ref(),
                &msg.param,
                SUMMARY_CHARACTERS,
                context,
            )
            .await,
        );

        self.timestamp = msg.get_timestamp();
        self.state = msg.state.into();
    }
}

pub async fn get_msg_info(context: &Context, msg_id: MsgId) -> Result<String, Error> {
    let mut ret = String::new();

    let msg = Message::load_from_db(context, msg_id).await?;

    let rawtxt: Option<String> = context
        .sql
        .query_value("SELECT txt_raw FROM msgs WHERE id=?;", paramsx![msg_id])
        .await?;

    let rawtxt = rawtxt.unwrap_or_default();
    let rawtxt = dc_truncate(rawtxt.trim(), 100_000);

    let fts = dc_timestamp_to_str(msg.get_timestamp());
    ret += &format!("Sent: {}", fts);

    let name = Contact::load_from_db(context, msg.from_id)
        .await
        .map(|contact| contact.get_name_n_addr())?;

    ret += &format!(" by {}", name);
    ret += "\n";

    if msg.from_id != DC_CONTACT_ID_SELF as u32 {
        let s = dc_timestamp_to_str(if 0 != msg.timestamp_rcvd {
            msg.timestamp_rcvd
        } else {
            msg.timestamp_sort
        });
        ret += &format!("Received: {}", &s);
        ret += "\n";
    }

    if msg.from_id == DC_CONTACT_ID_INFO || msg.to_id == DC_CONTACT_ID_INFO {
        // device-internal message, no further details needed
        return Ok(ret);
    }

    let pool = context.sql.get_pool().await?;

    let mut rows =
        sqlx::query_as("SELECT contact_id, timestamp_sent FROM msgs_mdns WHERE msg_id=?;")
            .bind(msg_id)
            .fetch(&pool);

    while let Some(row) = rows.next().await {
        let (contact_id, ts): (i32, i64) = row?;

        let fts = dc_timestamp_to_str(ts);
        ret += &format!("Read: {}", fts);

        let name = Contact::load_from_db(context, contact_id as u32)
            .await
            .map(|contact| contact.get_name_n_addr())?;

        ret += &format!(" by {}", name);
        ret += "\n";
    }

    ret += &format!("State: {}", msg.state);

    if msg.has_location() {
        ret += ", Location sent";
    }

    let e2ee_errors = msg.param.get_int(Param::ErroneousE2ee).unwrap_or_default();

    if 0 != e2ee_errors {
        if 0 != e2ee_errors & 0x2 {
            ret += ", Encrypted, no valid signature";
        }
    } else if 0 != msg.param.get_int(Param::GuaranteeE2ee).unwrap_or_default() {
        ret += ", Encrypted";
    }

    ret += "\n";
    if let Some(err) = msg.param.get(Param::Error) {
        ret += &format!("Error: {}", err)
    }

    if let Some(path) = msg.get_file(context) {
        let bytes = dc_get_filebytes(context, &path).await;
        ret += &format!("\nFile: {}, {}, bytes\n", path.display(), bytes);
    }

    if msg.viewtype != Viewtype::Text {
        ret += "Type: ";
        ret += &format!("{}", msg.viewtype);
        ret += "\n";
        ret += &format!("Mimetype: {}\n", &msg.get_filemime().unwrap_or_default());
    }
    let w = msg.param.get_int(Param::Width).unwrap_or_default();
    let h = msg.param.get_int(Param::Height).unwrap_or_default();
    if w != 0 || h != 0 {
        ret += &format!("Dimension: {} x {}\n", w, h,);
    }
    let duration = msg.param.get_int(Param::Duration).unwrap_or_default();
    if duration != 0 {
        ret += &format!("Duration: {} ms\n", duration,);
    }
    if !rawtxt.is_empty() {
        ret += &format!("\n{}\n", rawtxt);
    }
    if !msg.rfc724_mid.is_empty() {
        ret += &format!("\nMessage-ID: {}", msg.rfc724_mid);
    }
    if let Some(ref server_folder) = msg.server_folder {
        if server_folder != "" {
            ret += &format!("\nLast seen as: {}/{}", server_folder, msg.server_uid);
        }
    }

    Ok(ret)
}

pub fn guess_msgtype_from_suffix(path: &Path) -> Option<(Viewtype, &str)> {
    let extension: &str = &path.extension()?.to_str()?.to_lowercase();
    let info = match extension {
        "mp3" => (Viewtype::Audio, "audio/mpeg"),
        "aac" => (Viewtype::Audio, "audio/aac"),
        "mp4" => (Viewtype::Video, "video/mp4"),
        "webm" => (Viewtype::Video, "video/webm"),
        "jpg" => (Viewtype::Image, "image/jpeg"),
        "jpeg" => (Viewtype::Image, "image/jpeg"),
        "jpe" => (Viewtype::Image, "image/jpeg"),
        "png" => (Viewtype::Image, "image/png"),
        "webp" => (Viewtype::Image, "image/webp"),
        "gif" => (Viewtype::Gif, "image/gif"),
        "vcf" => (Viewtype::File, "text/vcard"),
        "vcard" => (Viewtype::File, "text/vcard"),
        _ => {
            return None;
        }
    };
    Some(info)
}

pub async fn get_mime_headers(context: &Context, msg_id: MsgId) -> Option<String> {
    context
        .sql
        .query_value(
            "SELECT mime_headers FROM msgs WHERE id=?;",
            paramsx![msg_id],
        )
        .await
        .ok()
}

pub async fn delete_msgs(context: &Context, msg_ids: &[MsgId]) {
    for msg_id in msg_ids.iter() {
        if let Ok(msg) = Message::load_from_db(context, *msg_id).await {
            if msg.location_id > 0 {
                delete_poi_location(context, msg.location_id).await;
            }
        }
        if let Err(err) = msg_id.trash(context).await {
            error!(context, "Unable to trash message {}: {}", msg_id, err);
        }
        job::add(
            context,
            job::Job::new(Action::DeleteMsgOnImap, msg_id.to_u32(), Params::new(), 0),
        )
        .await;
    }

    if !msg_ids.is_empty() {
        context.emit_event(Event::MsgsChanged {
            chat_id: ChatId::new(0),
            msg_id: MsgId::new(0),
        });
        job::kill_action(context, Action::Housekeeping).await;
        job::add(
            context,
            job::Job::new(Action::Housekeeping, 0, Params::new(), 10),
        )
        .await;
    }
}

async fn delete_poi_location(context: &Context, location_id: u32) -> bool {
    context
        .sql
        .execute(
            "DELETE FROM locations WHERE independent = 1 AND id=?;",
            paramsx![location_id as i32],
        )
        .await
        .is_ok()
}

pub async fn markseen_msgs(context: &Context, msg_ids: Vec<MsgId>) -> bool {
    if msg_ids.is_empty() {
        return false;
    }

    let mut send_event = false;
    for id in msg_ids.into_iter() {
        let query_res: Result<Option<(MessageState, Option<Blocked>)>, _> = context
            .sql
            .query_row_optional(
                r#"
SELECT
    m.state
    c.blocked
 FROM msgs m LEFT JOIN chats c ON c.id = m.chat_id
 WHERE m.id = ? AND m.chat_id > 9
"#,
                paramsx![id],
            )
            .await;

        if let Ok(Some((state, blocked))) = query_res {
            let blocked = blocked.unwrap_or_default();
            if blocked == Blocked::Not {
                if state == MessageState::InFresh || state == MessageState::InNoticed {
                    update_msg_state(context, id, MessageState::InSeen).await;
                    info!(context, "Seen message {}.", id);

                    job::add(
                        context,
                        job::Job::new(Action::MarkseenMsgOnImap, id.to_u32(), Params::new(), 0),
                    )
                    .await;
                    send_event = true;
                }
            } else if state == MessageState::InFresh {
                update_msg_state(context, id, MessageState::InNoticed).await;
                send_event = true;
            }
        }
    }

    if send_event {
        context.emit_event(Event::MsgsChanged {
            chat_id: ChatId::new(0),
            msg_id: MsgId::new(0),
        });
    }

    true
}

pub async fn update_msg_state(context: &Context, msg_id: MsgId, state: MessageState) -> bool {
    context
        .sql
        .execute(
            "UPDATE msgs SET state=? WHERE id=?;",
            paramsx![state, msg_id],
        )
        .await
        .is_ok()
}

pub async fn star_msgs(context: &Context, msg_ids: Vec<MsgId>, star: bool) -> bool {
    if msg_ids.is_empty() {
        return false;
    }

    for msg_id in msg_ids.into_iter() {
        if context
            .sql
            .execute(
                "UPDATE msgs SET starred=? WHERE id=?;",
                paramsx![star as i32, msg_id],
            )
            .await
            .is_err()
        {
            return false;
        }
    }

    true
}

/// Returns a summary test.
pub async fn get_summarytext_by_raw(
    viewtype: Viewtype,
    text: Option<impl AsRef<str>>,
    param: &Params,
    approx_characters: usize,
    context: &Context,
) -> String {
    let mut append_text = true;
    let prefix = match viewtype {
        Viewtype::Image => context.stock_str(StockMessage::Image).await.into_owned(),
        Viewtype::Gif => context.stock_str(StockMessage::Gif).await.into_owned(),
        Viewtype::Sticker => context.stock_str(StockMessage::Sticker).await.into_owned(),
        Viewtype::Video => context.stock_str(StockMessage::Video).await.into_owned(),
        Viewtype::Voice => context
            .stock_str(StockMessage::VoiceMessage)
            .await
            .into_owned(),
        Viewtype::Audio | Viewtype::File => {
            if param.get_cmd() == SystemMessage::AutocryptSetupMessage {
                append_text = false;
                context
                    .stock_str(StockMessage::AcSetupMsgSubject)
                    .await
                    .to_string()
            } else {
                let file_name: String = param
                    .get_path(Param::File, context)
                    .unwrap_or(None)
                    .and_then(|path| {
                        path.file_name()
                            .map(|fname| fname.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| String::from("ErrFileName"));
                let label = context
                    .stock_str(if viewtype == Viewtype::Audio {
                        StockMessage::Audio
                    } else {
                        StockMessage::File
                    })
                    .await;
                format!("{} – {}", label, file_name)
            }
        }
        _ => {
            if param.get_cmd() != SystemMessage::LocationOnly {
                "".to_string()
            } else {
                append_text = false;
                context.stock_str(StockMessage::Location).await.to_string()
            }
        }
    };

    if !append_text {
        return prefix;
    }

    let summary = if let Some(text) = text {
        if text.as_ref().is_empty() {
            prefix
        } else if prefix.is_empty() {
            dc_truncate(text.as_ref(), approx_characters).to_string()
        } else {
            let tmp = format!("{} – {}", prefix, text.as_ref());
            dc_truncate(&tmp, approx_characters).to_string()
        }
    } else {
        prefix
    };

    UNWRAP_RE.replace_all(&summary, " ").to_string()
}

// as we do not cut inside words, this results in about 32-42 characters.
// Do not use too long subjects - we add a tag after the subject which gets truncated by the clients otherwise.
// It should also be very clear, the subject is _not_ the whole message.
// The value is also used for CC:-summaries

// Context functions to work with messages

pub async fn exists(context: &Context, msg_id: MsgId) -> bool {
    if msg_id.is_special() {
        return false;
    }

    let chat_id: Option<ChatId> = context
        .sql
        .query_value("SELECT chat_id FROM msgs WHERE id=?;", paramsx![msg_id])
        .await
        .ok();

    if let Some(chat_id) = chat_id {
        !chat_id.is_trash()
    } else {
        false
    }
}

pub async fn set_msg_failed(context: &Context, msg_id: MsgId, error: Option<impl AsRef<str>>) {
    if let Ok(mut msg) = Message::load_from_db(context, msg_id).await {
        if msg.state.can_fail() {
            msg.state = MessageState::OutFailed;
        }
        if let Some(error) = error {
            msg.param.set(Param::Error, error.as_ref());
            warn!(context, "Message failed: {}", error.as_ref());
        }

        if context
            .sql
            .execute(
                "UPDATE msgs SET state=?, param=? WHERE id=?;",
                paramsx![msg.state, msg.param.to_string(), msg_id],
            )
            .await
            .is_ok()
        {
            context.emit_event(Event::MsgFailed {
                chat_id: msg.chat_id,
                msg_id,
            });
        }
    }
}

/// returns Some if an event should be send
pub async fn mdn_from_ext(
    context: &Context,
    from_id: u32,
    rfc724_mid: &str,
    timestamp_sent: i64,
) -> Option<(ChatId, MsgId)> {
    if from_id <= DC_MSG_ID_LAST_SPECIAL || rfc724_mid.is_empty() {
        return None;
    }

    let res: Result<(MsgId, ChatId, Chattype, MessageState), _> = context
        .sql
        .query_row(
            r#"
SELECT
    m.id AS msg_id,
    c.id AS chat_id,
    c.type AS type,
    m.state AS state
  FROM msgs m LEFT JOIN chats c ON m.chat_id=c.id
  WHERE rfc724_mid=? AND from_id=1
  ORDER BY m.id;"#,
            paramsx![rfc724_mid],
        )
        .await;
    if let Err(ref err) = res {
        info!(context, "Failed to select MDN {:?}", err);
    }

    if let Ok((msg_id, chat_id, chat_type, msg_state)) = res {
        let mut read_by_all = false;

        // if already marked as MDNS_RCVD msgstate_can_fail() returns false.
        // however, it is important, that ret_msg_id is set above as this
        // will allow the caller eg. to move the message away
        if msg_state.can_fail() {
            let mdn_already_in_table = context
                .sql
                .exists(
                    "SELECT contact_id FROM msgs_mdns WHERE msg_id=? AND contact_id=?;",
                    paramsx![msg_id, from_id as i32,],
                )
                .await
                .unwrap_or_default();

            if !mdn_already_in_table {
                context.sql.execute(
                    "INSERT INTO msgs_mdns (msg_id, contact_id, timestamp_sent) VALUES (?, ?, ?);",
                    paramsx![msg_id, from_id as i32, timestamp_sent],
                )
                    .await
                           .unwrap_or_default(); // TODO: better error handling
            }

            // Normal chat? that's quite easy.
            if chat_type == Chattype::Single {
                update_msg_state(context, msg_id, MessageState::OutMdnRcvd).await;
                read_by_all = true;
            } else {
                // send event about new state
                let ist_cnt: i32 = context
                    .sql
                    .query_value(
                        "SELECT COUNT(*) FROM msgs_mdns WHERE msg_id=?;",
                        paramsx![msg_id],
                    )
                    .await
                    .unwrap_or_default();
                let ist_cnt = ist_cnt as usize;

                /*
                Groupsize:  Min. MDNs

                1 S         n/a
                2 SR        1
                3 SRR       2
                4 SRRR      2
                5 SRRRR     3
                6 SRRRRR    3

                (S=Sender, R=Recipient)
                 */
                // for rounding, SELF is already included!
                let soll_cnt = (chat::get_chat_contact_cnt(context, chat_id).await + 1) / 2;
                if ist_cnt >= soll_cnt {
                    update_msg_state(context, msg_id, MessageState::OutMdnRcvd).await;
                    read_by_all = true;
                } // else wait for more receipts
            }
        }
        return if read_by_all {
            Some((chat_id, msg_id))
        } else {
            None
        };
    }
    None
}

/// The number of messages assigned to real chat (!=deaddrop, !=trash)
pub async fn get_real_msg_cnt(context: &Context) -> i32 {
    match context
        .sql
        .query_value(
            r#"
SELECT COUNT(*)
  FROM msgs m  LEFT JOIN chats c ON c.id=m.chat_id
  WHERE m.id>9 AND m.chat_id>9 AND c.blocked=0;
"#,
            paramsx![],
        )
        .await
    {
        Ok(res) => res,
        Err(err) => {
            error!(context, "dc_get_real_msg_cnt() failed. {}", err);
            0
        }
    }
}

pub async fn get_deaddrop_msg_cnt(context: &Context) -> usize {
    let res: Result<i32, _> = context
        .sql
        .query_value(
            r#"
SELECT COUNT(*)
  FROM msgs m LEFT JOIN chats c ON c.id=m.chat_id
  WHERE c.blocked=2;"#,
            paramsx![],
        )
        .await;
    match res {
        Ok(res) => res as usize,
        Err(err) => {
            error!(context, "dc_get_deaddrop_msg_cnt() failed. {}", err);
            0
        }
    }
}

pub async fn estimate_deletion_cnt(
    context: &Context,
    from_server: bool,
    seconds: i64,
) -> Result<usize, Error> {
    let self_chat_id = chat::lookup_by_contact_id(context, DC_CONTACT_ID_SELF)
        .await
        .unwrap_or_default()
        .0;
    let threshold_timestamp = time() - seconds;

    let cnt: i32 = if from_server {
        context
            .sql
            .query_value(
                r#"SELECT COUNT(*)
             FROM msgs m
             WHERE m.id > ?
               AND timestamp < ?
               AND chat_id != ?
               AND server_uid != 0;"#,
                paramsx![
                    DC_MSG_ID_LAST_SPECIAL as i32,
                    threshold_timestamp,
                    self_chat_id
                ],
            )
            .await?
    } else {
        context
            .sql
            .query_value(
                r#"SELECT COUNT(*)
             FROM msgs m
             WHERE m.id > ?
               AND timestamp < ?
               AND chat_id != ?
               AND chat_id != ? AND hidden = 0;"#,
                paramsx![
                    DC_MSG_ID_LAST_SPECIAL as i32,
                    threshold_timestamp,
                    self_chat_id,
                    ChatId::new(DC_CHAT_ID_TRASH)
                ],
            )
            .await?
    };
    Ok(cnt as usize)
}

/// Counts number of database records pointing to specified
/// Message-ID.
///
/// Unlinked messages are excluded.
pub async fn rfc724_mid_cnt(context: &Context, rfc724_mid: &str) -> i32 {
    // check the number of messages with the same rfc724_mid
    match context
        .sql
        .query_value(
            "SELECT COUNT(*) FROM msgs WHERE rfc724_mid=? AND NOT server_uid = 0",
            paramsx![rfc724_mid],
        )
        .await
    {
        Ok(res) => res,
        Err(err) => {
            error!(context, "dc_get_rfc724_mid_cnt() failed. {}", err);
            0
        }
    }
}

pub(crate) async fn rfc724_mid_exists(
    context: &Context,
    rfc724_mid: &str,
) -> Result<Option<(String, u32, MsgId)>, Error> {
    if rfc724_mid.is_empty() {
        warn!(context, "Empty rfc724_mid passed to rfc724_mid_exists");
        return Ok(None);
    }

    let res: Option<(Option<String>, i32, MsgId)> = context
        .sql
        .query_row_optional(
            "SELECT server_folder, server_uid, id FROM msgs WHERE rfc724_mid=?",
            paramsx![rfc724_mid],
        )
        .await?;

    Ok(res.map(|(a, b, c)| (a.unwrap_or_default(), b as u32, c)))
}

pub async fn update_server_uid(
    context: &Context,
    rfc724_mid: &str,
    server_folder: impl AsRef<str>,
    server_uid: u32,
) {
    match context
        .sql
        .execute(
            "UPDATE msgs SET server_folder=?, server_uid=? WHERE rfc724_mid=?",
            paramsx![server_folder.as_ref(), server_uid as i32, rfc724_mid],
        )
        .await
    {
        Ok(_) => {}
        Err(err) => {
            warn!(context, "msg: failed to update server_uid: {}", err);
        }
    }
}

#[allow(dead_code)]
pub async fn dc_empty_server(context: &Context, flags: u32) {
    job::kill_action(context, Action::EmptyServer).await;
    job::add(
        context,
        job::Job::new(Action::EmptyServer, flags, Params::new(), 0),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils as test;

    #[test]
    fn test_guess_msgtype_from_suffix() {
        assert_eq!(
            guess_msgtype_from_suffix(Path::new("foo/bar-sth.mp3")),
            Some((Viewtype::Audio, "audio/mpeg"))
        );
    }

    #[async_std::test]
    async fn test_prepare_message_and_send() {
        use crate::config::Config;

        let d = test::dummy_context().await;
        let ctx = &d.ctx;

        let contact = Contact::create(ctx, "", "dest@example.com")
            .await
            .expect("failed to create contact");

        let res = ctx
            .set_config(Config::ConfiguredAddr, Some("self@example.com"))
            .await;
        assert!(res.is_ok());

        let chat = chat::create_by_contact_id(ctx, contact).await.unwrap();

        let mut msg = Message::new(Viewtype::Text);

        let msg_id = chat::prepare_msg(ctx, chat, &mut msg).await.unwrap();

        let _msg2 = Message::load_from_db(ctx, msg_id).await.unwrap();
        assert_eq!(_msg2.get_filemime(), None);
    }

    #[async_std::test]
    async fn test_get_summarytext_by_raw() {
        let d = test::dummy_context().await;
        let ctx = &d.ctx;

        let some_text = Some("bla bla".to_string());
        let empty_text = Some("".to_string());
        let no_text: Option<String> = None;

        let mut some_file = Params::new();
        some_file.set(Param::File, "foo.bar");

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Text, some_text.as_ref(), &Params::new(), 50, &ctx)
                .await,
            "bla bla" // for simple text, the type is not added to the summary
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Image, no_text.as_ref(), &some_file, 50, &ctx).await,
            "Image" // file names are not added for images
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Video, no_text.as_ref(), &some_file, 50, &ctx).await,
            "Video" // file names are not added for videos
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Gif, no_text.as_ref(), &some_file, 50, &ctx,).await,
            "GIF" // file names are not added for GIFs
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Sticker, no_text.as_ref(), &some_file, 50, &ctx,)
                .await,
            "Sticker" // file names are not added for stickers
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Voice, empty_text.as_ref(), &some_file, 50, &ctx,)
                .await,
            "Voice message" // file names are not added for voice messages, empty text is skipped
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Voice, no_text.as_ref(), &mut some_file, 50, &ctx)
                .await,
            "Voice message" // file names are not added for voice messages
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Voice, some_text.as_ref(), &some_file, 50, &ctx).await,
            "Voice message \u{2013} bla bla" // `\u{2013}` explicitly checks for "EN DASH"
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Audio, no_text.as_ref(), &mut some_file, 50, &ctx)
                .await,
            "Audio \u{2013} foo.bar" // file name is added for audio
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Audio, empty_text.as_ref(), &some_file, 50, &ctx,)
                .await,
            "Audio \u{2013} foo.bar" // file name is added for audio, empty text is not added
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::Audio, some_text.as_ref(), &some_file, 50, &ctx).await,
            "Audio \u{2013} foo.bar \u{2013} bla bla" // file name and text added for audio
        );

        assert_eq!(
            get_summarytext_by_raw(Viewtype::File, some_text.as_ref(), &mut some_file, 50, &ctx)
                .await,
            "File \u{2013} foo.bar \u{2013} bla bla" // file name is added for files
        );

        let mut asm_file = Params::new();
        asm_file.set(Param::File, "foo.bar");
        asm_file.set_cmd(SystemMessage::AutocryptSetupMessage);
        assert_eq!(
            get_summarytext_by_raw(Viewtype::File, no_text.as_ref(), &mut asm_file, 50, &ctx).await,
            "Autocrypt Setup Message" // file name is not added for autocrypt setup messages
        );
    }
}
