// TODO: only open one dialog at a time?

use cosmic::iced::Subscription;
use cosmic::iced::futures::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use zbus::zvariant;

use crate::components::polkit_dialog;

const OBJECT_PATH: &str = "/com/system76/CosmicOsd";

pub fn subscription(system_connection: zbus::Connection) -> Subscription<Event> {
    struct Wrapper {
        id: &'static str,
        conn: zbus::Connection,
    }

    impl Hash for Wrapper {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            self.id.hash(state);
        }
    }

    Subscription::run_with(
        Wrapper {
            id: "dbus-polkit-agent",
            conn: system_connection,
        },
        |Wrapper {
             id: _id,
             conn: system_connection,
         }| {
            let connection = system_connection.clone();
            async move {
                let (sender, receiver) = mpsc::channel(32);
                tokio::spawn(async move {
                    if let Err(e) = serve_agent(&connection, sender).await {
                        log::warn!("Failed to serve the PolicyKit agent: {}", e);
                    }
                });
                ReceiverStream::new(receiver)
            }
            .flatten_stream()
        },
    )
}

#[derive(Clone, Debug)]
pub enum Event {
    CreateDialog(polkit_dialog::Params),
    CancelDialog { cookie: String },
}

#[allow(dead_code)]
#[derive(Clone, Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum PolkitError {
    Failed,
    Cancelled,
    NotSupported,
    NotAuthorized,
    CancellationIdNotUnique,
}

#[derive(serde::Serialize)]
pub struct Subject<'a> {
    subject_kind: &'a str,
    subject_details: HashMap<&'a str, zvariant::Value<'a>>,
}

impl<'a> zvariant::Type for Subject<'a> {
    const SIGNATURE: &'static zvariant::Signature =
        <(String, HashMap<String, zvariant::Value>)>::SIGNATURE;
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Identity<'a> {
    identity_kind: &'a str,
    identity_details: HashMap<&'a str, zvariant::Value<'a>>,
}

impl<'a> zvariant::Type for Identity<'a> {
    const SIGNATURE: &'static zvariant::Signature =
        <(String, HashMap<String, zvariant::Value>)>::SIGNATURE;
}

#[zbus::proxy(
    default_service = "org.freedesktop.login1",
    interface = "org.freedesktop.login1.Session",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait LogindSession {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    default_service = "org.freedesktop.PolicyKit1",
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait PolkitAuthority {
    fn register_authentication_agent(
        &self,
        subject: Subject<'_>,
        locale: &str,
        object_path: &str,
    ) -> zbus::Result<()>;
    fn unregister_authentication_agent(
        &self,
        subject: Subject<'_>,
        object_path: &str,
    ) -> zbus::Result<()>;
}

struct PolkitAgent {
    sender: mpsc::Sender<Event>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl PolkitAgent {
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        identities: Vec<Identity<'_>>,
    ) -> Result<(), PolkitError> {
        if let Some((_uid, pw_name)) = select_user_from_identities(&identities) {
            let (response_sender, response_receiver) = oneshot::channel();
            let icon_name = if !icon_name.is_empty() {
                Some(icon_name)
            } else {
                None
            };
            let _ = self
                .sender
                .send(Event::CreateDialog(polkit_dialog::Params {
                    pw_name,
                    action_id,
                    message,
                    icon_name,
                    details,
                    cookie,
                    response_sender: Arc::new(Mutex::new(Some(response_sender))),
                }))
                .await;
            response_receiver.await.unwrap()
        } else {
            Err(PolkitError::Failed)
        }
    }

    async fn cancel_authentication(&self, cookie: String) -> Result<(), PolkitError> {
        let _ = self.sender.send(Event::CancelDialog { cookie }).await;
        Ok(())
    }
}

fn select_user_from_identities(identities: &[Identity]) -> Option<(u32, String)> {
    let mut uids = Vec::new();
    for ident in identities {
        if ident.identity_kind == "unix-user"
            && let Some(zvariant::Value::U32(uid)) = ident.identity_details.get("uid")
        {
            uids.push(*uid);
        }
        // `unix-group` is apparently a thing too, but Gnome Shell doesn't seem to handle it...
    }

    // Like Gnome Shell, try own uid, then root, then first UID in `identities`
    let uid = *uids
        .iter()
        .find(|uid| **uid == uzers::get_current_uid())
        .or(uids.iter().find(|uid| **uid == 0))
        .or_else(|| uids.first())?;

    let user = uzers::get_user_by_uid(uid)?;
    Some((uid, user.name().to_str()?.to_string()))
}

/// Serve the agent and register it with polkit, then again with every polkitd
/// that takes over: a restarted polkitd, as an OS update leaves it, has
/// forgotten every agent, and no dialog would show until the next login.
async fn serve_agent(
    system_connection: &zbus::Connection,
    sender: mpsc::Sender<Event>,
) -> zbus::Result<()> {
    let agent = PolkitAgent { sender };
    system_connection
        .object_server()
        .at(OBJECT_PATH, agent)
        .await?;

    let session = LogindSessionProxy::new(system_connection).await?;
    let session_id = session.id().await?;
    let authority = PolkitAuthorityProxy::new(system_connection).await?;
    // Watched before the first registration, so a restart in between is seen.
    let mut owners = authority.inner().receive_owner_changed().await?;

    register_agent(&authority, &session_id).await;
    while let Some(owner) = owners.next().await {
        if owner.is_some() {
            log::info!("polkit restarted; registering the PolicyKit agent again");
            register_agent(&authority, &session_id).await;
        }
    }
    Ok(())
}

async fn register_agent(authority: &PolkitAuthorityProxy<'_>, session_id: &str) {
    let mut subject_details = HashMap::new();
    subject_details.insert("session-id", session_id.into());
    let subject = Subject {
        subject_kind: "unix-session",
        subject_details,
    };

    // XXX locale
    if let Err(e) = authority
        .register_authentication_agent(subject, "en_US", OBJECT_PATH)
        .await
    {
        log::warn!(
            "Failed to register PolicyKit agent: {}. This is normal if an agent is already registered.",
            e
        );
    }
}
