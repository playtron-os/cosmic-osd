//! Lets another surface ask the OSD to stay out of its way.
//!
//! The panel's volume and brightness popups already show a slider for the value
//! they are changing. An OSD firing for that same change puts two controls for
//! one action on screen at once, which is what this exists to stop: while a
//! popup is driving the value it calls `SuppressFor`, and value OSDs are held
//! back until that window lapses.
//!
//! It is a window rather than an on/off latch on purpose — a client that dies
//! mid-drag, or forgets to release, cannot wedge the OSD off permanently.

use cosmic::iced::Subscription;
use cosmic::iced::futures::FutureExt;
use std::hash::Hash;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const OBJECT_PATH: &str = "/com/system76/CosmicOsd";

/// Longest window a client may ask for, so a bad value can't silence the OSD
/// for the rest of the session.
const MAX_SUPPRESS_MILLIS: u64 = 5_000;

#[derive(Clone, Debug)]
pub enum Event {
    /// Hold value OSDs back for this many milliseconds from now.
    SuppressFor(u64),
}

pub fn subscription(connection: zbus::Connection) -> Subscription<Event> {
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
            id: "dbus-osd-inhibit",
            conn: connection,
        },
        |Wrapper { id: _id, conn }| {
            let connection = conn.clone();
            async move {
                let (sender, receiver) = mpsc::channel(32);
                tokio::spawn(async move {
                    if let Err(e) = serve(&connection, sender).await {
                        log::warn!("failed to serve the OSD inhibit interface: {e}");
                    }
                });
                ReceiverStream::new(receiver)
            }
            .flatten_stream()
        },
    )
}

struct OsdInhibit {
    sender: mpsc::Sender<Event>,
}

#[zbus::interface(name = "com.system76.CosmicOsd.Inhibit")]
impl OsdInhibit {
    /// Hold back value OSDs (volume, brightness) for `millis`, capped at
    /// [`MAX_SUPPRESS_MILLIS`]. Call it repeatedly to keep a drag covered.
    async fn suppress_for(&self, millis: u64) {
        let _ = self
            .sender
            .send(Event::SuppressFor(millis.min(MAX_SUPPRESS_MILLIS)))
            .await;
    }
}

async fn serve(connection: &zbus::Connection, sender: mpsc::Sender<Event>) -> zbus::Result<()> {
    connection
        .object_server()
        .at(OBJECT_PATH, OsdInhibit { sender })
        .await?;
    Ok(())
}
